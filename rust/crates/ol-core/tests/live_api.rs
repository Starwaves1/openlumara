//! Live end-to-end test of the core against a real OpenAI-compatible server.
//! Ignored by default; run with:
//!   OL_LIVE_API=http://192.168.1.5:18080/v1 cargo test -p ol-core --test live_api -- --ignored --nocapture

use futures::StreamExt;
use ol_core::channel::{Channel, ChannelBehavior, SendArgs, TokenStream};
use ol_core::module::{Module, ModuleBase, ParamType, ToolArgs, ToolParam, ToolSpec};
use ol_core::registry::{ModuleDescriptor, Registry};
use serde_json::{json, Value};
use std::sync::Arc;

struct TestChannel;

#[async_trait::async_trait]
impl ChannelBehavior for TestChannel {
    async fn on_stream(&self, _ch: &Arc<Channel>, mut stream: TokenStream) -> anyhow::Result<()> {
        while let Some(t) = stream.next().await {
            if t["type"] == "content" {
                print!("{}", t["content"].as_str().unwrap_or(""));
            }
        }
        println!();
        Ok(())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Adder {
    base: ModuleBase,
}

#[async_trait::async_trait]
impl Module for Adder {
    fn base(&self) -> &ModuleBase {
        &self.base
    }
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec::new(
            "add",
            "Adds two integers and returns the sum.\n\nArgs:\n    a: first number\n    b: second number",
            vec![ToolParam::req("a", ParamType::Integer), ToolParam::req("b", ParamType::Integer)],
        )]
    }
    async fn call_tool(&self, method: &str, args: ToolArgs) -> anyhow::Result<Value> {
        assert_eq!(method, "add");
        Ok(self.base.result(json!(args.i64("a").unwrap_or(0) + args.i64("b").unwrap_or(0)), true))
    }
    async fn on_system_prompt(&self) -> anyhow::Result<Option<String>> {
        Ok(Some("You can add numbers with the adder_add tool.".into()))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn live_round_trip() {
    let Ok(url) = std::env::var("OL_LIVE_API") else { return };
    let dir = tempfile::tempdir().unwrap();
    ol_core::paths::set_project_root(dir.path());
    let mut reg = Registry::new();
    reg.add_module(ModuleDescriptor {
        name: "adder",
        class_name: "Adder",
        doc: "adds",
        settings: || json!({}),
        unsafe_: false,
        category: "",
        factory: |base| Arc::new(Adder { base }),
    });
    ol_core::registry::install(reg);
    ol_core::config::load(None).unwrap();

    // discover the model id
    let models: Value = reqwest::get(format!("{}/models", url)).await.unwrap().json().await.unwrap();
    let model = models["data"][0]["id"].as_str().unwrap().to_string();
    ol_core::config::update(|c| {
        c["api"]["url"] = json!(url);
        c["model"]["name"] = json!(model);
        c["model"]["enable_thinking"] = json!(false);
        c["model"]["preloaded_modules"] = json!(["adder"]);
        c["modules"]["enabled"] = json!(["adder"]);
    });

    let manager = ol_core::manager::Manager::new(Default::default());
    let ch = Channel::new(&manager, "test", false, Arc::new(TestChannel)).unwrap();
    // replicate the relevant parts of manager.run() without spawning channel tasks
    ch.init().await;
    ch.set_as_active_channel().await;
    manager.insert_channel(ch.clone());
    let base = ModuleBase::new("adder", false, Arc::downgrade(&manager));
    let adder: Arc<dyn Module> = Arc::new(Adder { base });
    manager.insert_module(adder);
    ch.tool_loader.refresh_meta_tool_descriptions();
    ch.tool_loader.load_default_tools();
    assert!(ch.tool_loader.active_names().contains(&"adder_add".to_string()));

    // plain chat round trip
    let mut s = ch.send_stream(json!("Reply with exactly the word: pong"), SendArgs::authorized());
    let mut content = String::new();
    while let Some(t) = s.next().await {
        if t["type"] == "content" {
            content.push_str(t["content"].as_str().unwrap_or(""));
        }
        if t["type"] == "error" {
            panic!("error token: {}", t);
        }
    }
    println!("assistant: {}", content);
    assert!(content.to_lowercase().contains("pong"));
    let history = ch.chat.messages_get();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["role"], "user");
    assert_eq!(history[1]["role"], "assistant");

    // tool call round trip
    let mut s = ch.send_stream(json!("Use the adder_add tool to compute 1234 + 4321, then tell me the result."), SendArgs::authorized());
    let mut saw_tool = false;
    let mut content = String::new();
    while let Some(t) = s.next().await {
        match t["type"].as_str() {
            Some("tool") => {
                println!("tool response: {}", t["content"]);
                assert!(t["content"].as_str().unwrap().contains("5555"));
                saw_tool = true;
            }
            Some("content") => content.push_str(t["content"].as_str().unwrap_or("")),
            Some("error") => panic!("error token: {}", t),
            _ => {}
        }
    }
    println!("assistant: {}", content);
    assert!(saw_tool);
    assert!(content.contains("5555") || content.contains("5,555"));
    let history = ch.chat.messages_get();
    let roles: Vec<&str> = history.iter().map(|m| m["role"].as_str().unwrap_or("?")).collect();
    println!("roles: {:?}", roles);
    assert_eq!(&roles[2..], &["user", "assistant", "tool", "assistant"]);

    // command round trip
    let r = ch.send(json!("/ping"), SendArgs::authorized()).await.unwrap();
    assert_eq!(r["content"], "pong!");
}
