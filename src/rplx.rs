use std::{io::{Error, ErrorKind}, process};
use crate::{
    ecies::{ECIESDirection, HandshakeSecrets, ECIES},
    messages::{Capability, Disconnect, Hello, Ping, Pong, RLPx_Message, Status},
};
use aes::cipher::{generic_array::GenericArray, BlockDecrypt, BlockEncrypt, KeyInit};
use alloy_primitives::B512;
use alloy_rlp::{Buf, BytesMut, Encodable,Decodable};
use ctr::cipher::KeyIvInit;
use ctr::cipher::StreamCipher;
use ethereum_types::{H128, H256};
use log::{debug, error, info, warn};
use rlp::RlpStream;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use sha2::{Digest, Sha256};
use sha3::Keccak256;
use snap::raw::Decoder as SnapDecoder;
use tokio_util::codec::{Decoder, Encoder};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RlpxState {
    ExpectingConnection,
    AuthSent,
    AuthAckRecieved,
    HelloSent,
    HelloRecieved,
    Active,
    Disconnected,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FrameState {
    DecodingHeader,
    DecodingFrame(usize),
}

#[derive(Clone)]
pub struct RLPx {
    rlpx_state: RlpxState,
    direction: ECIESDirection,
    auth_request: BytesMut,
    ecies: ECIES,
    public_key: PublicKey,
    frame_state: FrameState,
    secrets: Option<HandshakeSecrets>,
}

pub const PROTOCOL_VERSION: usize = 5;
const ZERO_HEADER: &[u8; 16] = &[0, 0, 148, 194, 128, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // Lifted from geth


const FRAME_HEADER_CIPHERTEXT_SIZE: usize = 16;
const FRAME_MAC_SIZE: usize = 16;

impl RLPx {
    pub fn new(our_private_key: SecretKey, peer_public_key: PublicKey) -> Self {
        let public_key = PublicKey::from_secret_key(SECP256K1, &our_private_key);
        Self {
            rlpx_state: RlpxState::ExpectingConnection,
            direction: ECIESDirection::Outgoing,
            auth_request: BytesMut::new(), // todo
            ecies: ECIES::new(our_private_key, peer_public_key),
            public_key: public_key,
            frame_state: FrameState::DecodingHeader,
            secrets: None,
        }
    }

    fn write_frame(&mut self, data: &[u8]) -> BytesMut {
        // frame = header-ciphertext || header-mac || frame-ciphertext || frame-mac
        // header = frame-size || header-data || header-padding
        // header-data = [capability-id, context-id]

        // header = frame-size || header-data || header-padding
        let mut header_buf = BytesMut::new();
        header_buf.extend_from_slice(ZERO_HEADER);
        // We're missing a byte from the length here.
        let x: u16 = data.len() as u16;
        header_buf[1..3].copy_from_slice(&x.to_be_bytes());

        let secrets = self.secrets.as_mut().unwrap();

        // header-ciphertext = aes(aes-secret, header)
        secrets
            .aes_keystream_egress
            .apply_keystream(header_buf.as_mut());
        // header-mac-seed = aes(mac-secret, keccak256.digest(egress-mac)[:16]) ^ header-ciphertext
        let egress_mac = &secrets.egress_mac.clone().finalize();
        let mut egress_mac_digest: [u8; 16] = [0; 16];
        egress_mac_digest.copy_from_slice(&egress_mac[..16]);
        secrets
            .mac_secret
            .encrypt_block(GenericArray::from_mut_slice(egress_mac_digest.as_mut()));

        let mut header_mac_seed: [u8; 16] = [0; 16];
        for i in 0..header_mac_seed.len() {
            header_mac_seed[i] = egress_mac_digest[i] ^ header_buf[i];
        }

        // egress-mac = keccak256.update(egress-mac, header-mac-seed)
        // header-mac = keccak256.digest(egress-mac)[:16]
        secrets.egress_mac.update(header_mac_seed);
        let header_mac = &secrets.egress_mac.clone().finalize()[..16];

        let mut out = BytesMut::default();
        out.reserve(32);
        out.extend_from_slice(&header_buf);
        out.extend_from_slice(header_mac);

        let mut len = data.len();
        if len % 16 > 0 {
            len = (len / 16 + 1) * 16;
        }

        let old_len = out.len();
        out.resize(old_len + len, 0);

        let encrypted = &mut out[old_len..old_len + len];
        encrypted[..data.len()].copy_from_slice(data);

        //frame-ciphertext = aes(aes-secret, frame-data || frame-padding)
        secrets.aes_keystream_egress.apply_keystream(encrypted);
        // egress-mac = keccak256.update(egress-mac, frame-ciphertext)
        secrets.egress_mac.update(encrypted);
        // frame-mac-seed = aes(mac-secret, keccak256.digest(egress-mac)[:16]) ^ keccak256.digest(egress-mac)[:16]
        // keccak256.digest(egress-mac)[:16])
        let egress_mac = &secrets.egress_mac.clone().finalize();
        let mut egress_mac_digest: [u8; 16] = [0; 16];
        egress_mac_digest.copy_from_slice(&egress_mac[0..16]);
        let mut egress_mac_aes = egress_mac_digest.clone();
        // This is done in block encryption mode
        //aes(mac-secret, keccak256.digest(egress-mac)[:16])
        secrets
            .mac_secret
            .encrypt_block(GenericArray::from_mut_slice(egress_mac_aes.as_mut()));
        let mut frame_mac_seed: [u8; 16] = [0; 16];
        for i in 0..frame_mac_seed.len() {
            frame_mac_seed[i] = egress_mac_aes[i] ^ egress_mac_digest[i];
        }

        // egress-mac = keccak256.update(egress-mac, frame-mac-seed)
        secrets.egress_mac.update(frame_mac_seed);

        // frame-mac = keccak256.digest(egress-mac)[:16]
        let frame_mac = &secrets.egress_mac.clone().finalize()[..16];

        out.extend_from_slice(frame_mac);

        out
    }

    pub fn decode_frame_header<'a>(
        &mut self,
        data_in: &'a mut [u8],
    ) -> Result<usize, &'static str> {

        // frame = header-ciphertext || header-mac || frame-ciphertext || frame-mac
        let (header_ciphertext, rest) = data_in
            .split_at_mut_checked(FRAME_HEADER_CIPHERTEXT_SIZE)
            .ok_or("No header ciphertext! ")?;

        let (header_mac, rest) = rest
            .split_at_mut_checked(FRAME_MAC_SIZE)
            .ok_or("No header MAC ")?;

        // Get a local reference so it's simpler and I don't have to unwrap it every time.
        let secrets = self.secrets.as_mut().unwrap();

        // According to https://github.com/ethereum/devp2p/blob/master/rlpx.md the handshake works like this:
        // header-mac-seed = aes(mac-secret, keccak256.digest(egress-mac)[:16]) ^ header-ciphertext
        // egress-mac = keccak256.update(egress-mac, header-mac-seed)
        // header-mac = keccak256.digest(egress-mac)[:16]
        let ingress_mac = &secrets.ingress_mac.clone().finalize();
        // debug!("ingress_mac: {:?}", ingress_mac);

        let mut ingress_mac_digest: [u8; 16] = [0; 16];

        ingress_mac_digest.copy_from_slice(&ingress_mac[..16]);
        // debug!("ingress_mac_digest: {:?}", ingress_mac_digest);

        secrets
            .mac_secret
            .encrypt_block(GenericArray::from_mut_slice(ingress_mac_digest.as_mut()));
        // debug!("ingress_mac_digest block encrypted with AES: {:?}", ingress_mac_digest);

        let mut header_mac_seed: [u8; 16] = [0; 16];
        for i in 0..header_mac_seed.len() {
            header_mac_seed[i] = ingress_mac_digest[i] ^ header_ciphertext[i];
        }
        // debug!("header_mac_seed: {:?}", header_mac_seed);

        // egress-mac = keccak256.update(egress-mac, header-mac-seed)
        // header-mac = keccak256.digest(egress-mac)[:16]
        secrets.ingress_mac.update(header_mac_seed);
        // debug!("egress-mac digest full: {:?}", &secrets.ingress_mac.clone().finalize());

        let header_mac_computed = &secrets.ingress_mac.clone().finalize()[..16];
        // debug!("header_ciphertext: {:?}", header_ciphertext);
        // debug!("header_mac_computed: {:?}", header_mac_computed);
        // debug!("header_mac:  {:?}", header_mac);
        if header_mac_computed != header_mac {
            return Err("Header MAC mismatch!");
        }

        secrets
            .aes_keystream_ingress
            .apply_keystream(header_ciphertext);

        let mut payload_size = u32::from_be_bytes([0, header_ciphertext[0], header_ciphertext[1], header_ciphertext[2]]) as usize;

        if (payload_size % 16) !=0 { 
            payload_size = ((payload_size / 16) +1)*16;
        }
        Ok(payload_size)

    }

    pub fn decode_frame_ciphertext<'a>(
        &mut self,
        data_in: &'a mut [u8],
    ) -> Result<&'a mut [u8], &'static str> {
        const FRAME_MAC_SIZE: usize = 16;

        let (frame_ciphertext, frame_mac) = data_in
            .split_at_mut_checked((data_in.len() - FRAME_MAC_SIZE))
            .ok_or("No frame MAC, invalid frame length ")?;

        // Get a local reference so it's simpler and I don't have to unwrap it every time.
        let secrets = self.secrets.as_mut().unwrap();

        // egress-mac = keccak256.update(egress-mac, frame-ciphertext)
        secrets.ingress_mac.update(&frame_ciphertext);
        // frame-mac-seed = aes(mac-secret, keccak256.digest(egress-mac)[:16]) ^ keccak256.digest(egress-mac)[:16]
        // keccak256.digest(egress-mac)[:16])
        let ingress_mac = &secrets.ingress_mac.clone().finalize();
        let mut ingress_mac_digest: [u8; 16] = [0; 16];
        ingress_mac_digest.copy_from_slice(&ingress_mac[0..16]);
        let mut ingress_mac_aes = ingress_mac_digest.clone();
        // This is done in block encryption mode
        //aes(mac-secret, keccak256.digest(egress-mac)[:16])
        secrets
            .mac_secret
            .encrypt_block(GenericArray::from_mut_slice(ingress_mac_aes.as_mut()));
        let mut frame_mac_seed: [u8; 16] = [0; 16];
        for i in 0..frame_mac_seed.len() {
            frame_mac_seed[i] = ingress_mac_aes[i] ^ ingress_mac_digest[i];
        }

        // egress-mac = keccak256.update(egress-mac, frame-mac-seed)
        secrets.ingress_mac.update(frame_mac_seed);

        // frame-mac = keccak256.digest(egress-mac)[:16]
        let frame_mac_computed = &secrets.ingress_mac.clone().finalize()[..16];

        if frame_mac_computed != frame_mac {
            debug!("RX Frame MAC mismatch!");
            return Err("Frame MAC mismatch!");
        }

        secrets
            .aes_keystream_ingress
            .apply_keystream(frame_ciphertext);

        Ok(frame_ciphertext)
    }

    pub fn get_state(&self) -> RlpxState {
        self.rlpx_state
    }

    pub fn hello_msg(&mut self) -> BytesMut {
        let msg = Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: "reth_hello".to_string(),
            capabilities: vec![Capability {
                version: 68,
                name: "eth".to_string(),
            }],
            port: 0,
            id: *B512::from_slice(&self.public_key.serialize_uncompressed()[1..]),
        };

        let mut encoded_hello = BytesMut::default();
        Hello::ID.encode(&mut encoded_hello);
        msg.encode(&mut encoded_hello);

        self.write_frame(&encoded_hello)
    }

    fn decode_frame(&mut self, src: &mut BytesMut) -> Result<Option<RLPx_Message>, std::io::Error> {
        
        if self.frame_state == FrameState::DecodingHeader{
            if src.len() >= FRAME_HEADER_CIPHERTEXT_SIZE+FRAME_MAC_SIZE {
                let frame_ciphertext_size = self.decode_frame_header(src).map_err(|err|{
                    error!("Error decoding header: {:?} ", err);
                    Error::from(ErrorKind::Other)})?;

                self.frame_state = FrameState::DecodingFrame(frame_ciphertext_size);
                src.advance(FRAME_HEADER_CIPHERTEXT_SIZE+FRAME_MAC_SIZE);
            }
            else {
                // Call us back until we get a full header. 
                return Ok(None);
            }
        }
        return match self.frame_state {
            FrameState::DecodingFrame(frame_ciphertext_size) => {

                if src.len() >= frame_ciphertext_size {

                    let decrypted_frame = self.decode_frame_ciphertext(&mut src[..frame_ciphertext_size + FRAME_MAC_SIZE]).map_err(|err|{
                        error!("Error decrypting frame: {:?} ", err);
                        Error::from(ErrorKind::Other)})?;
                        
                    let message_id =  self.decode_frame_data(decrypted_frame).unwrap();
                    src.advance(frame_ciphertext_size+FRAME_MAC_SIZE);
                    self.frame_state = FrameState::DecodingHeader;

                    Ok(Some(message_id))
                }
                else {
                    // Call us back until we get a full header. 
                    Ok(None)
                }

            },
            _ => {
                error!(" Unexpected state! We should not have gotten in this situation! ");
                Err(Error::from(ErrorKind::Other)) }
        }
    }

    fn decode_frame_data(&mut self, frame: &[u8]) -> Result<RLPx_Message,  &'static str> {

        let (message_id, message) = frame.split_at(1);
        let message_id = u8::decode(&mut &message_id[..]).map_err(|_|" RLP stream decode error! ")?;

        debug!("Message ID received: {}", message_id);

        match message_id{
            Hello::ID => {
                let hello = Hello::decode(&mut &message[..]);
                info!("Hello message recieved from target node: {:?}", hello.unwrap());
                return Ok(RLPx_Message::Hello);
            },

            _ => {
                info!("We probably got back the eth capabily status message, we can't handle it currently, just exit.");
                process::exit(0);
                return Err("TODO")
            }
        }
    }
}

impl Encoder<RLPx_Message> for RLPx {
    type Error = std::io::Error;

    fn encode(&mut self, item: RLPx_Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        debug!("Encoding message {:?}!", item);
        match item {
            RLPx_Message::Auth => {
                dst.clear();

                dst.extend_from_slice(self.ecies.get_auth_request());

                self.rlpx_state = RlpxState::AuthSent;
            }
            RLPx_Message::AuthAck => {
                // Implement AuthAck encoding here
                todo!()
            }
            RLPx_Message::Hello => {
                dst.extend_from_slice(&self.hello_msg());
            }
            RLPx_Message::Disconnect(reason) => {
                todo!()
            }
            RLPx_Message::Ping => {
                todo!()
            }
            RLPx_Message::Pong => {
                todo!()
            }
            RLPx_Message::Status(msg) => {
                todo!()
            }
        }
        Ok(())
    }
}

impl Decoder for RLPx {
    type Item = RLPx_Message;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // See example here:
        // https://docs.rs/tokio-util/latest/tokio_util/codec/index.html
        //   It seems we need to validate full frame and clear only the frame
        // data we processed. There are some issues with what I'm doing here,
        // so caveat emptor. To be addressed after handshake works properly.
        //   We proooobably need to process header before frame data in order to
        // insure frame integrity
        if src.is_empty() {
            return Ok(None);
        }
        match self.rlpx_state {
            RlpxState::AuthSent => {
                debug!("We're decoding authAck... ");

                // debug!("We're decoding !! Raw Data is: {:?} ", src);
                let (_decrypted, frame_size) = self
                    .ecies
                    .decrypt(src)
                    .map_err(|e| {debug!("Frame decrypt Error: {:?}", e); Error::from(ErrorKind::Other)})?;

                self.secrets = Some(self.ecies.get_secrets());
                self.rlpx_state = RlpxState::AuthAckRecieved;
                // debug!("Raw data after Ack rx buffer is:  {:?} ", src.as_mut());
                src.advance(frame_size);

                self.frame_state = FrameState::DecodingHeader;

                return Ok(Some(RLPx_Message::AuthAck));
            }
            RlpxState::AuthAckRecieved => {
                debug!("We're decoding a Hello frame... ");

                return match self.decode_frame(src) {
                    Ok(Some(RLPx_Message::Hello)) =>  {
                        self.rlpx_state = RlpxState::Active;
                        Ok(Some(RLPx_Message::Hello))
                    },
                    Ok(None) => {Ok(None)}
                    _ => {
                        error!(" Unexpected message! We should have gotten a Hello!! ");
                        Err(Error::from(ErrorKind::Other))
                    },
                }
            
            }
            RlpxState::Active => {
                debug!("We're decoding a protocol frame... ");

                return self.decode_frame(src)

            }
            _ => {
                debug!("Invalid frame!! ");
                return Err(Error::from(ErrorKind::Other));
            }
        }
    }
}
