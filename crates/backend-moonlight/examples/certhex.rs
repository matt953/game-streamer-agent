//! Print the stored dev identity's certificate as the hex the `/pair`
//! endpoint expects, for reproducing a phase-1 request by hand.

fn main() {
    let path = std::env::var("GSA_MOONLIGHT_KEY").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("moonlight-dev-key.pem")
            .display()
            .to_string()
    });
    let pem = std::fs::read_to_string(&path).expect("stored identity");
    let id = gsa_backend_moonlight::ClientIdentity::from_key_pem(&pem).expect("load identity");
    print!("{}", gsa_backend_moonlight::cert_pem_hex(&id));
}
