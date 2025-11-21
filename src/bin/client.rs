use chrono::Local;
use std::env;
use std::error::Error;
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::net::TcpStream;
use std::thread;

fn main() -> Result<(), Box<dyn Error>> {
    // Usage: cargo run --bin client -- 127.0.0.1:5000
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5000".to_string());

    let mut stream =
        TcpStream::connect(&addr).map_err(|e| format!("failed to connect to {}: {}", addr, e))?;
    println!("Connected to server: {}", addr);

    let client_addr = stream.local_addr()?;
    println!("Client address {:?}", client_addr);

    let port_string = client_addr.to_string();

    let client_id = port_string
        .split(':')
        .last()
        .ok_or_else(|| "failed to get port number")?;
    println!("Port number of this client is cliend_id {:?}", client_id);

    // We can clone the stream to handle reading and writing halfs of the tcp connection to the client.
    // To read messages from the server we can spawn a thread

    // Clone stream for reading thread
    let mut read_stream = stream
        .try_clone()
        .map_err(|e| format!("failed to clone stream: {}", e))?;
    // Spawn thread to read messages from server — handle errors inside thread (closure returns ())
    let reader_handle = thread::spawn(move || -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut reader = BufReader::new(&mut read_stream);
        loop {
            let mut line = String::new();
            // n is the number of bytes read from stream using the buf reader
            // For bufreader i used for reading from stream, blocks when waiting to read from stream
            // only reads it when there is some data being sent from server or
            // if the connection is closed then the number of bytes read is 0
            let n = reader.read_line(&mut line)?; // Here the io::Error is propagated using ? 
            if n == 0 {
                println!("Server closed connection. Please exit.");
                break;
            };
            print!("{}", line); // server messages that are stored in the line i read using bufreader
        }
        Ok(())
    });

    // Main thread: read stdin input given by the client user and send to server
    // If client wants to quit he can type /quit or quit in CLI
    let stdin = io::stdin();
    for line_res in stdin.lines() {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("/quit") || trimmed.eq_ignore_ascii_case("quit") {
            println!("Quitting and closing client connection...");
            // When client wants to quit we shutdown both read and write halves of the tcp connection
            // or else the reader thread will be blocked waiting to read from stream
            stream.shutdown(Shutdown::Both)?;
            break;
        }
        // Client just sends raw text; server will add timestamp & client_id
        let send_line = format!("{}\n", trimmed);
        if let Err(e) = stream.write_all(send_line.as_bytes()) {
            eprintln!("failed to send message: {}", e);
            break;
        }
        // Show clients own message to themself with timestamp
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        println!("[{}] You sent: {}", ts, trimmed);
    }

    // close stream by dropping its memory
    drop(stream);

    // Run join on the join_handle of the thread so that the thread is not dropped before the main function.
    // Receive errors generated within the spawned thread and match them, so you can give exact errors to the main thread.
    match reader_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("reader thread error: {}", e),
        Err(_) => eprintln!("reader thread panicked"),
    }

    Ok(())
}
