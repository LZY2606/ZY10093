use std::net::TcpListener;
use std::path::PathBuf;

use bfw::seed::seed;
use bfw::server_http::HttpServer;
use bfw::store::Store;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut listen = "127.0.0.1:5219".to_string();
    let mut data_dir = PathBuf::from("bfw-data");
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                if i < args.len() {
                    listen = args[i].clone();
                }
            }
            "--data-dir" => {
                i += 1;
                if i < args.len() {
                    data_dir = PathBuf::from(&args[i]);
                }
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let store = Store::open(&data_dir).unwrap_or_else(|e| {
        eprintln!("failed to open data dir {}: {e}", data_dir.display());
        std::process::exit(1);
    });
    if let Err(e) = seed(&store) {
        eprintln!("failed to seed workspace: {}", serde_json::to_string(&e.body).unwrap_or_default());
        std::process::exit(1);
    }

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        eprintln!("failed to bind {listen}: {e}");
        std::process::exit(1);
    });
    eprintln!("Binary Format Workbench listening on http://{listen}");
    eprintln!("data directory: {}", data_dir.display());
    let server = HttpServer::new(store);
    server.serve(listener).unwrap_or_else(|e| {
        eprintln!("server error: {e}");
        std::process::exit(1);
    });
}
