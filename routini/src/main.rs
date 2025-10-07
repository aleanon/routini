use pingora::prelude::RoundRobin;
use routini::application::Application;
use std::net::TcpListener;

fn main() {
    let listener = TcpListener::bind("127.0.0.1:3500").unwrap();
    let backends = [
        "127.0.0.1:4000".to_owned(),
        "127.0.0.1:4001".to_owned(),
        "127.0.0.1:4002".to_owned(),
    ];

    Application::<RoundRobin>::new(listener, backends).run();
}
