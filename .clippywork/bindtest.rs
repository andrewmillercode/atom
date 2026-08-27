use std::net::TcpListener;
fn main() {
    match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => println!("bind ok: {}", l.local_addr().unwrap()),
        Err(e) => println!("bind failed: {e}"),
    }
}
