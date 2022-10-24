use anyhow::Result;
use base64::encode;
use clap::{Arg, Command};
use openssl::x509::X509;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sigstore::oauth;
use std::io::Read;
use std::path::PathBuf;
use std::{fs::File, io::Write};
use tokio::task;

use sigstore::crypto::SigningScheme;

mod crypto;
mod rekor_api;
extern crate question;

const FULCIO_URL: &str = "https://fulcio.sigstore.dev/api/v1/signingCert";
const SIGSTORE_OAUTH_URL: &str = "https://oauth2.sigstore.dev/auth";

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FulcioPayload {
    pub public_key: PubKey,
    pub signed_email_address: String,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PubKey {
    pub algorithm: String,
    pub content: String,
}
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let matches = Command::new("ferris-sign")
        .version("0.1")
        .author("Luke Hinds")
        .about("Simple rust based example of sigstore signing")
        .arg(
            Arg::new("sign")
                .short('s')
                .long("sign")
                .requires("in-file")
                .requires("sig-out")
                .requires("cert-out")
                .takes_value(false)
                .help("OIDC sign"),
        )
        .arg(
            Arg::new("cert-out")
                .short('c')
                .long("cert-out")
                .takes_value(true)
                .help("Location to place signing certificate output"),
        )
        .arg(
            Arg::new("in-file")
                .short('i')
                .long("in-file")
                .takes_value(true)
                .help("Location of file to sign"),
        )
        .arg(
            Arg::new("sig-out")
                .short('o')
                .long("sig-out")
                .takes_value(true)
                .help("Location to place signature output"),
        )
        .arg(
            Arg::new("extract")
            .short('e')
            .long("extract")
            .takes_value(true)
            .help("Extract public key from Fulcio signing certificate")
        )
        .get_matches();

    let signer = SigningScheme::ECDSA_P256_SHA256_ASN1.create_signer()?;

    if matches.is_present("extract") {

        // TODO: should this functionality be added to sigstore-rs?

        let cert_file = matches.value_of("extract").unwrap();
        let mut file = File::open(cert_file)?;
        let mut cert_data = Vec::new();
        file.read_to_end(&mut cert_data)?;

        let certificate = X509::from_pem(&cert_data)?;
        let pub_key_pem = certificate.public_key()?.public_key_to_pem()?;
        let pub_key_pem_string = String::from_utf8(pub_key_pem)?;

        println!("Extracted public key from Fulcio signing certificate file...\n");
        println!("{:?}", pub_key_pem_string);
    }

    if matches.is_present("sign") {
        let in_filename = matches.value_of("in-file").unwrap();
        let sig_filename = matches.value_of("sig-out").unwrap();
        let cert_filename = matches.value_of("cert-out").unwrap();

        // use tokio::task::spawn_blocking to call OpenIDAuthorize in a blocking thread
        let oidc_url = task::spawn_blocking(move || {
            oauth::openidflow::OpenIDAuthorize::new(
                "sigstore",
                "",
                SIGSTORE_OAUTH_URL,
                "http://localhost:8080",
            )
            .auth_url()
            .unwrap()
        })
        .await?;

        if open::that(oidc_url.0.to_string()).is_ok() {
            println!(
                "Open this URL in a browser if it does not automatically open for you:\n{}\n",
                oidc_url.0
            );
        }

        // use tokio::task::spawn_blocking to call RedirectListener in a blocking thread
        let result = task::spawn_blocking(move || {
            oauth::openidflow::RedirectListener::new(
                "127.0.0.1:8080",
                oidc_url.1, // client
                oidc_url.2, // nonce
                oidc_url.3, //
            )
            .redirect_listener()
            .unwrap()
        })
        .await?;

        // use tokio::task::spawn_blocking to call RedirectListener in a blocking thread
        let result = task::spawn_blocking(move || result).await?;

        let (token_response, id_token) = result;
        let email = token_response.email().unwrap();
        println!("Received token for email scope: {:?}", email);

        let signature = signer.sign(email.to_string().as_bytes()).unwrap();

        let key_pair = signer.to_sigstore_keypair()?;
        let public_key_pem = key_pair.public_key_to_pem()?;
        let params = FulcioPayload {
            public_key: PubKey {
                content: encode(&public_key_pem),
                algorithm: String::from("ecdsa"),
            },
            signed_email_address: encode(&signature),
        };

        let body = serde_json::to_string(&params).unwrap();
        println!("Requesting signing certificate from Fulcio...");

        let client = reqwest::Client::new();
        let response = client
            .post(FULCIO_URL)
            .header("Authorization", format!("Bearer {}", id_token.to_string()))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;
        let certs = response.text().await?;

        let mut cert_pem = String::new();

        let cert_re =
            Regex::new(r#"-----BEGIN CERTIFICATE-----([^-]*)-----END CERTIFICATE-----"#).unwrap();
        for capture in cert_re.find_iter(&String::from_utf8(certs.as_bytes().to_vec()).unwrap()) {
            let cert = openssl::x509::X509::from_pem(capture.as_str().as_bytes()).unwrap();
            for jk in cert.issuer_name().entries() {
                if matches.is_present("cert-out") {
                    // print the value of file
                    if jk.data().as_slice() == b"sigstore-intermediate" {
                        let filename = matches.value_of("cert-out").unwrap();
                        let mut file = File::create(filename).unwrap();
                        cert_pem.push_str(capture.as_str());
                        file.write_all(capture.as_str().as_bytes()).unwrap();
                    }
                }
            }
        }
        println!(
            "Saving signing cerificate to {}",
            matches.value_of("cert-out").unwrap()
        );

        // sign in-file contents
        let mut file = File::open(in_filename).unwrap();
        let mut file_bytes = Vec::new();
        file.read_to_end(&mut file_bytes).unwrap();
        let signature = signer.sign(&file_bytes).unwrap();

        // write signature to file
        let mut file = File::create(sig_filename).unwrap();
        file.write_all(&signature).unwrap();
        println!("Saving signature to {}", sig_filename);

        // read in bytes from cert file
        let mut cert_file_open = File::open(cert_filename).unwrap();
        let mut cert_file_bytes = Vec::new();
        cert_file_open.read_to_end(&mut cert_file_bytes).unwrap();

        // send to rekor, converting signature and cert to base64
        let signature_base64 = encode(&signature);
        let cert_file_base64 = encode(&cert_file_bytes);

        let hash = crypto::sha256_digest(PathBuf::from(in_filename))?;

        println!("Sending signature artifacts to rekor...");
        let log_entry = rekor_api::create_log(&hash, &cert_file_base64, &signature_base64).await;
        println!("{:#?}", log_entry);

        // retrieve same entry from rekor
        let uuid = log_entry.unwrap().uuid;
        let retrieved_entry = rekor_api::get_entry_by_uuid(&uuid).await.unwrap();
        println!("Retrieved log entry from Rekor by UUID... {}", uuid);
        println!("{:#?}", retrieved_entry);
    }
    anyhow::Ok(())
}
