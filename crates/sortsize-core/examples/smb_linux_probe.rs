//! Opt-in native desktop qualification. Only explicitly supplied lab addresses.
//! No file enumeration, contents or credential parameters are accepted.
#[cfg(target_os = "linux")]
fn main() {
    use serde_json::json;
    use sortsize_core::volumes::{smb, smb_disconnect};
    use std::path::Path;
    let args: Vec<_> = std::env::args().skip(1).collect();
    let Some(operation) = args.first() else {
        eprintln!(
            "usage: smb_linux_probe parse|list|resolve|connect|reconnect|disconnect|guard TARGET"
        );
        std::process::exit(2);
    };
    let Some(value) = args.get(1) else {
        std::process::exit(2)
    };
    // Every command uses a validated credential-free URI or a mounted POSIX
    // location. Invalid input itself is deliberately not echoed to diagnostics.
    let path = Path::new(value);
    let result = match operation.as_str() {
        "guard" => {
            use sortsize_core::acquisition::{Guard, Policy};
            let output = Guard::open(path,Policy {same_volume:true, explicit_root:true});
            match output {
                Ok(_) => {println!("{}",json!({"status":"GuardOpened"}));return;}
                Err(e) => {println!("{}",json!({"error":format!("{:?}",e.kind()),"os_code":e.raw_os_error()}));std::process::exit(1);}
            }
        }
        "parse" => smb::parse_target(value).map(|t|json!({"server": t.server, "share": t.share})),
        "list" => smb::parse_target(value).and_then(|t|smb::enumerate_shares(&t))
            .map(|s|json!({"status":format!("{:?}",s.status()),"limited":s.limited,"shares":s.shares.iter().map(|s|json!({"name":s.name,"uri":s.path})).collect::<Vec<_>>()})),
        "resolve" => smb::resolve_path(path).map(|native|json!({"native":native,"logical":smb::logical_path(&native)})),
        "connect"|"reconnect" => smb::reconnect(path,false,0).and_then(|()|smb::resolve_path(path)).map(|native|json!({"status":"Connected","native":native})),
        "disconnect" => {
            let output = smb_disconnect::prepare(path,false).and_then(|ticket|smb_disconnect::disconnect(&ticket));
            match output {
                Ok(()) => {println!("{}",json!({"status":"Disconnected"})); return;}
                Err(e) => {println!("{}",json!({"error":format!("{e:?}")}));std::process::exit(1);}
            }
        }
        _ => {std::process::exit(2)}
    };
    match result {
        Ok(output) => println!("{output}"),
        Err(e) => {
            println!(
                "{}",
                json!({"status":format!("{:?}",e.status),"os_code":e.os_code})
            );
            std::process::exit(1);
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("This opt-in probe requires native Linux.");
    std::process::exit(2);
}
