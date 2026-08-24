//! CLI smoke test: streams one prompt through the active deck.
//! Usage: cargo run -p itsjustcad-deck --example chat -- "make three boxes"

use itsjustcad_deck::{make_deck, system_prompt, ChatMessage, ChatRequest, DeckDelta, DecksFile, Role};

#[tokio::main]
async fn main() {
    let prompt = std::env::args().nth(1).unwrap_or("make a 4x4x3 box at the origin".into());
    let decks = DecksFile::load_or_default();
    let config = &decks.decks[decks.active];
    eprintln!("deck: {} ({} @ {})", config.name, config.model, config.base_url);

    let deck = make_deck(config);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let req = ChatRequest::text(
        system_prompt("", &itsjustcad_commands::PluginRegistry::new()),
        vec![ChatMessage { role: Role::User, content: prompt }],
        String::new(),
        2048,
        0.2,
        None,
    );
    tokio::spawn(async move { deck.stream_chat(req, tx).await });

    while let Some(delta) = rx.recv().await {
        match delta {
            DeckDelta::Text(t) => print!("{t}"),
            DeckDelta::Session(sid) => eprintln!("session: {sid}"),
            DeckDelta::Done => {
                println!("\n--- done");
                break;
            }
            DeckDelta::Error(e) => {
                println!("\n--- error: {e}");
                break;
            }
        }
    }
}
