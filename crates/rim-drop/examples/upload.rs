//! cargo run -p rim-drop --features client --example upload -- <drop url> <file> [page]
//! Encrypts and uploads a file like RogueIM does and prints the link. The token
//! comes from RIM_DROP_TOKEN.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: upload <drop url> <file> [page]");
        std::process::exit(2);
    }
    let token = std::env::var("RIM_DROP_TOKEN").expect("RIM_DROP_TOKEN");
    let page = a.get(3).map(String::as_str).unwrap_or("https://cyberhirsch.github.io/RogueIM/f/");
    match rim_drop::client::upload(&a[1], &token, page, std::path::Path::new(&a[2]), 7 * 86_400, 0, |s, t| eprint!("\r{s} / {t}")) {
        Ok(u) => println!("\n{}", u.link),
        Err(e) => {
            eprintln!("\n{e}");
            std::process::exit(1);
        }
    }
}
