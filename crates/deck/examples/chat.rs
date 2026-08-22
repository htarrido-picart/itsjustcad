//! CLI smoke test: streams one prompt through the active deck.
//! Usage: cargo run -p mydrafter-deck --example chat -- "make three boxes"

use mydrafter_deck::{make_deck, system_prompt, ChatMessage, ChatRequest, DeckDelta, DecksFile, Role};

#[tokio::main]
async fn main() {
    let prompt = std::env::args().nth(1).unwrap_or("make a 4x4x3 box at the origin".into());
    let decks = DecksFile::load_or_default();
    let config = &decks.decks[decks.active];
    eprintln!("deck: {} ({} @ {})", config.name, config.model, config.base_url);

    let deck = make_deck(config);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let req = ChatRequest {
        system: system_prompt(""),
        messages: vec![ChatMessage { role: Role::User, content: prompt }],
        model: String::new(),
        max_tokens: 2048,
        temperature: 0.2,
    };
    tokio::spawn(async move { deck.stream_chat(req, tx).await });

    while let Some(delta) = rx.recv().await {
        match delta {
            DeckDelta::Text(t) => print!("{t}"),
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
