use std::io::{Read, Write};
use std::net::TcpStream;

fn main() -> std::io::Result<()> {
    let mut stream = TcpStream::connect("example.com:80")?;
    println!("connected to example.com on port 80");

    let request = "GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
    stream.write_all(request.as_bytes())?;

    let mut response = Vec::new();
    let mut buffer = [0u8; 4096];
    
    loop {
        let bytes_read = stream.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }

        response.extend_from_slice(&buffer[..bytes_read]);
    }

    println!("{}", String::from_utf8_lossy(&response));
    Ok(())
}
