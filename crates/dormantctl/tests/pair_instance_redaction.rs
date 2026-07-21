use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::thread;

use assert_cmd::Command;
use dormant_core::ipc_proto::{CoordinationDiscoveredPeer, CoordinationPeers, IpcResponse};

#[test]
fn pair_instance_never_prints_private_material() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("dormant.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let sentinel_code = "SENTINEL-CODE-DO-NOT-PRINT";
    let sentinel_key = "SENTINEL-PRIVATE-KEY-DO-NOT-PRINT";

    let server = thread::spawn(move || {
        for response in [
            IpcResponse::coordination_peers(CoordinationPeers {
                discovered: vec![CoordinationDiscoveredPeer {
                    instance_id: "peer-id".into(),
                    display_name: "Office Mac".into(),
                    pairing_port: 4242,
                    window_id: sentinel_key.into(),
                }],
                paired: vec![],
            }),
            IpcResponse::ok(None),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        }
    });

    let output = Command::cargo_bin("dormantctl")
        .unwrap()
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "pair",
            "instance",
            "Office Mac",
            "--code",
            sentinel_code,
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stdout={stdout}; stderr={stderr}");
    assert!(!stdout.contains(sentinel_code) && !stderr.contains(sentinel_code));
    assert!(!stdout.contains(sentinel_key) && !stderr.contains(sentinel_key));
}
