use chrono::Local;
use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender},
};
use std::thread::{self, JoinHandle};

#[derive(Clone)]
struct Client {
    id: i32,
    addr: SocketAddr,
    writer: Arc<Mutex<TcpStream>>,
}

#[derive(Debug)]
enum ServerMessage {
    Broadcast {
        from_id: i32,
        text: String,
        timestamp: String,
    },
    ClientJoined {
        id: i32,
        timestamp: String,
    },
    ClientLeft {
        id: i32,
        timestamp: String,
    },
}

fn log(msg: &str) {
    println!("[{}] {}", Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
}

/// Read loop for a single client. This function is running for single client, in a dedicated thread
/// to receive all the messages received from the client tcp stream
/// This function interacts with the broadcasting thread using channels sender tx to send message to be
/// broadcasted to broadcast thread's receiver rx.
///  Uses `?` for errors and returns Result so the thread can propagate.
fn handle_client_read(
    mut read_stream: TcpStream,
    id: i32,
    tx: Sender<ServerMessage>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut reader = BufReader::new(read_stream.try_clone()?);

    // As soon as we handle a client read stream for the first time,
    // We need to notify other clients that a new client has joined.
    // So we pass message to the broadcaster thread from this client read thread.
    let ts_join = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    tx.send(ServerMessage::ClientJoined {
        id,
        timestamp: ts_join,
    })
    .map_err(|e| format!("failed to send join msg: {}", e))?;

    // We keep checking if client has sent server any message by looping
    // the read_line function blocks the reader stream
    loop {
        let mut line = String::new();

        // This functions keeps checking until a new line is reached from client
        let n = reader.read_line(&mut line)?;
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

        if n == 0 {
            // When the reader is disconnected we get number of bytes read as zero
            // As the client got disconnected we pass a message to the broadcaster thread from the client read thread
            // to broadcast a message that the client has left
            let _ = tx.send(ServerMessage::ClientLeft { id, timestamp });
            break;
        }

        let text = line.trim_end_matches(&['\n', '\r'][..]).to_string();
        // If the client tries to send empty spaces in a line we try to ignore them and keep waiting for other messages
        if text.is_empty() {
            continue;
        }
        // We are broadcasting the message by sending it through a channel from
        // Handle_client_read thread ---> tx-channel-rx ---> broadcasting thread
        tx.send(ServerMessage::Broadcast {
            from_id: id.clone(),
            text,
            timestamp,
        })
        .map_err(|e| format!("failed to send broadcast msg: {}", e))?;
    }

    // Ensure the reader stream is dropped
    drop(reader);
    drop(read_stream);

    Ok(())
}

/// Broadcaster thread: receives ServerMessage and writes to client's write streams.
/// Exits when all Senders are dropped (i.e. rx.recv() returns Err).
fn broadcaster_loop(
    rx: Receiver<ServerMessage>,
    // Shared state of all connected clients, provides write streams to send messages to clients
    clients_arc: Arc<Mutex<Vec<Client>>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    while let Ok(msg) = rx.recv() {
        match msg {
            // We have 3 types of broadcast messages
            // 1. Client Joined
            // 2. Client Left
            // 3. Client has sent message to send to everyone else.
            ServerMessage::ClientJoined { id, timestamp: _ } => {
                log(&format!("Client joined: Client:{} ", id));
                let notification = format!("Server: Client:{} joined the chat\n", id);
                {
                    // We take a lock over mutex to fetch all the client streams
                    // So that we can broadcast message by looping
                    let clients = clients_arc
                        .lock()
                        .map_err(|e| format!("clients lock poisoned: {}", e))?;

                    // Running a loop over all the clients that are alive with the shared state of threads
                    // Always leaving out sender to receive his own message
                    for client in clients.iter() {
                        if client.id == id {
                            continue; // don't notify the joiner
                        }

                        // Taking lock over the clients writer stream which we cloned in main thread
                        // Writing notification that a new client has joined
                        // When we don't obtain lock due to other thread getting error,
                        // the lock become poisoned, and returns Poison error which we propagate using ?
                        let mut w = client
                            .writer
                            .lock()
                            .map_err(|e| format!("writer lock poisoned: {}", e))?;
                        // Sending string data as bytes to the stream, this will be received by the client end of tcp stream
                        w.write_all(notification.as_bytes())?;
                    }
                }
            }
            ServerMessage::ClientLeft { id, timestamp: _ } => {
                // Sending this message to every other client after a client disconnects
                log(&format!("Client disconnected: Client:{}", id));
                {
                    // Remove the client from shared state between threads so that he receives no more message broadcasts
                    // Do this before broadcasting so that other messages are not impacted due to client removal
                    let mut clients = clients_arc
                        .lock()
                        .map_err(|e| format!("clients lock poisoned: {}", e))?;
                    // With retain we only have client ids which are still alive
                    clients.retain(|c| c.id != id);
                    log(&format!("Total number of clients live: {}", clients.len()));
                }

                // Notify others
                let notification = format!("Server: Client:{} left the chat\n", id);
                {
                    let clients = clients_arc
                        .lock()
                        .map_err(|e| format!("clients lock poisoned: {}", e))?;
                    for client in clients.iter() {
                        let mut w = client
                            .writer
                            .lock()
                            .map_err(|e| format!("writer lock poisoned: {}", e))?;

                        w.write_all(notification.as_bytes())?;
                    }
                }
            }
            // Main logic of sending message to all clients (Broadcast)
            ServerMessage::Broadcast {
                from_id,
                text,
                timestamp,
            } => {
                let sender_name = format!("Client:{}", from_id);
                // First show in the server what it has received from the client and then
                // Broadcast it to all the clients connected
                log(&format!("Received from {}: {}", sender_name, text));
                let to_send = format!("[{}] {}: {}\n", timestamp, sender_name, text);
                {
                    let clients = clients_arc
                        .lock()
                        .map_err(|e| format!("clients lock poisoned: {}", e))?;
                    for client in clients.iter() {
                        if client.id == from_id {
                            continue; // don't send message again to sender
                        }
                        let mut w = client
                            .writer
                            .lock()
                            .map_err(|e| format!("writer lock poisoned: {}", e))?;
                        w.write_all(to_send.as_bytes())?;
                    }
                }
            }
        }
    }

    // rx.recv() returned Err -> that means all client read thread channel senders are dropped -> graceful shutdown
    log("Broadcaster exiting: all client read thread senders dropped. Shutting down broadcasting");
    Ok(())
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // Accept port as first CLI argument, this we use to start the server or else use the default port
    let port = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "5000".to_string()); // default as string

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
        .map_err(|e| format!("failed to bind on port {}: {}", port, e))?;
    log(&format!("Server started on port {}", port));

    // Shared clients list to share between all threads of broadcasting.
    // Every time a new client joins we add it to this list of Clients
    // Which is used by the broadcaster thread to send messages to all client's write streams
    let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));

    // let mut clients_number: i32 = 0;

    // Channel for broadcasting messages, rx from this is sent inside broadcaster thread so it receives from the client_read threads
    // and delivers to all client threads.
    let (channel_sender, channel_receiver) = mpsc::channel::<ServerMessage>();

    // We are using 1 broadcaster thread to send all messages to all clients (write streams)
    // We use seperate client_read thread for each reading message from each client
    // == 1 Broadcaster thread
    // == (number_clients) * 1 thread each for reading received messages

    // Spawn broadcaster thread and store its JoinHandle, we do this inorder to make sure
    // that broadcaster thread is dropped before the main thread.
    let clients_for_broadcaster = Arc::clone(&clients);
    let broadcaster_handle: JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> =
        thread::spawn(move || broadcaster_loop(channel_receiver, clients_for_broadcaster));

    // Every time a new client joins, we create a thread for reading the clients input.
    // The join handles returned are then added this vector of clients.
    let mut client_handles: Vec<JoinHandle<Result<(), Box<dyn Error + Send + Sync>>>> = Vec::new();

    // This is the main TCP loop which keeps checking incoming connections
    // If a new connection arrives then a client address and port is fetched
    // We use atomicusize
    for stream_res in listener.incoming() {
        match stream_res {
            Ok(stream) => {
                // let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
                let peer_addr = stream
                    .peer_addr()
                    .map_err(|e| format!("failed to get peer addr: {}", e))?;

                let client_address_string = peer_addr.to_string();

                // Trying to fetch port number from the client address to use as client id
                let client_id = client_address_string
                    .clone()
                    .split(':')
                    .last()
                    .ok_or_else(|| "failed to get port number")?
                    .parse::<i32>()
                    .map_err(|e| format!("failed to parse port number as i32: {}", e))?;

                log(&format!(
                    "Client connected: {} (id: {})",
                    peer_addr, client_id
                ));
                // println!("Port number of this client is cliend_id {:?}", client_id);

                // Every time we get a new client we fetch his tcpstream and clone it
                // So we have one write stream and another read stream
                // Create writer Arc for this client (store cloned stream)
                let writer =
                    Arc::new(Mutex::new(stream.try_clone().map_err(|e| {
                        format!("failed to clone stream for storing writer: {}", e)
                    })?));

                let client = Client {
                    id: client_id,
                    addr: peer_addr,
                    writer: Arc::clone(&writer),
                };

                // Insert client into shared list
                // Here we are trying to acquire lock over clients mutex
                // we create a new scope with brackets to drop the lock early
                // This will reduce lock waiting or contention between different threads
                {
                    let mut clients_locked = clients
                        .lock()
                        .map_err(|e| format!("clients lock poisoned: {}", e))?;
                    clients_locked.push(client);
                    log(&format!(
                        "Total number of clients connected live: {}",
                        clients_locked.len()
                    ));
                }

                // Creating clones of channel senders (tx) to send inside spawned thread of client
                // These senders are used to communicated received messages to broadcaster thread receiver (rx)
                // We move clones instead of references to make sure the values are not dropped
                // while they are used in the thread.
                let channel_sender_clone = channel_sender.clone();

                // Clone stream for reading
                let read_stream = stream.try_clone().map_err(|e| {
                    format!("failed to clone stream for reader thread creation: {}", e)
                })?;

                // Spawn supervised client handler thread and collect its JoinHandle.
                // Since i want to handle all the errors generated inside a thread properly
                // I take the joinhandle returned by the thread and use it to fetch errors
                // The closure being used inside is FnOnce and It returns errors which are
                // implementing dynamic trait aand also thread safe by implementing marker traits Send + Sync
                let handle: JoinHandle<Result<(), Box<dyn Error + Send + Sync>>> =
                    thread::spawn(move || {
                        if let Err(e) =
                            handle_client_read(read_stream, client_id, channel_sender_clone)
                        {
                            Err(e) // I am simply propagating the error
                        } else {
                            Ok(())
                        }
                    });

                // We store all the join handles received in a vector to handle client thread errors properly
                client_handles.push(handle);
            }
            Err(e) => {
                // Accept error (temporary or permanent). Log and continue or break based on error type.
                eprintln!("failed to accept client: {}", e);
            }
        }
    }

    // Accept loop ended (listener closed or unrecoverable error). Begin shutdown:
    // Drop the original Sender so broadcaster will exit after all client threads finish and drop their channel_sender clones.
    drop(channel_sender);

    // Join all client threads and report errors/panics
    for handle in client_handles {
        match handle.join() {
            Ok(Ok(())) => {} // thread completed successfully
            Ok(Err(e)) => eprintln!("client thread returned error: {}", e),
            Err(join_err) => eprintln!("client thread panicked: {:?}", join_err),
        }
    }

    // Join broadcaster thread and handle errors
    match broadcaster_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("broadcaster thread returned error: {}", e),
        Err(join_err) => eprintln!("broadcaster thread panicked: {:?}", join_err),
    }

    log("Server shutdown completed.");

    Ok(())
}
