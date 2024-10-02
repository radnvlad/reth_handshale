use log::{error, info, warn, debug};
use secp256k1::{PublicKey, SecretKey};
use std::{env, fmt::Error, net::TcpStream, net::IpAddr, str::FromStr};

fn main() {
    env_logger::init();
    let peers_eip = match get_peers() {
        Ok(x) => {x}
        Err(e) => {error!("Error getting peers! {}",e); return}

    };

    for (public_key, ip_address) in peers_eip{
        establish_session(public_key, ip_address);
    }
}

fn get_peers() -> Result<Vec<(PublicKey, String)>, &'static str>
{
    const ENODE_PREFIX:&str = "enode://";

    let mut nodes: Vec<(PublicKey, String)> = Vec::new();

    for enode in env::args().skip(1){

        debug!("Args are: {:?}", enode);

        let (enode_prefix, enode_data) = 
        match enode.split_once(ENODE_PREFIX) {
            Some(x) => x,
            None => return Err("Invalid enode prefix! "),
        };

        if enode_prefix != "" {
            return Err("Invalid enode prefix location! ")
        }

        let (enode_key_string, ip_address) = 
            match enode_data.rsplit_once("@") {
                Some(x) => x,
                None => return Err("Invalid ip address"),
            };

        let mut enode_key_string =   enode_key_string.to_string();
        enode_key_string.insert_str(0, "04");

        let enode_public_key = match PublicKey::from_str(&enode_key_string){
            Ok(e) => e,
            Err(x) => {
                error!("Key parse err is: {:?}", x);
                return Err("Invalid enode public key ")
            }
        };
        nodes.push((enode_public_key, ip_address.to_string()));
    }
    Ok(nodes)
}


fn establish_session(public_key: PublicKey, ip_address: String)
{
    match TcpStream::connect(&ip_address) {
        Ok(mut stream) => {}
        Err(e) => {}
    }
}