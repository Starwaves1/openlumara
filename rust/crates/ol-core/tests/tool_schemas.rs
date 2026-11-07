//! Regression test: the tool schemas built from the generated module metadata must be
//! byte-identical (python json.dumps) to what the python tool loader (python 3.14) produces.
//! Fixture produced by running core.tool_loader on every python module class.

#[path = "../../../generated/modules/auto_backup.rs"]
mod m_auto_backup;
#[path = "../../../generated/modules/calculator.rs"]
mod m_calculator;
#[path = "../../../generated/modules/calendar.rs"]
mod m_calendar;
#[path = "../../../generated/modules/channel.rs"]
mod m_channel;
#[path = "../../../generated/modules/characters.rs"]
mod m_characters;
#[path = "../../../generated/modules/chats.rs"]
mod m_chats;
#[path = "../../../generated/modules/coder.rs"]
mod m_coder;
#[path = "../../../generated/modules/docs.rs"]
mod m_docs;
#[path = "../../../generated/modules/file_manager.rs"]
mod m_file_manager;
#[path = "../../../generated/modules/http.rs"]
mod m_http;
#[path = "../../../generated/modules/identity.rs"]
mod m_identity;
#[path = "../../../generated/modules/lists.rs"]
mod m_lists;
#[path = "../../../generated/modules/mcp.rs"]
mod m_mcp;
#[path = "../../../generated/modules/memory.rs"]
mod m_memory;
#[path = "../../../generated/modules/models.rs"]
mod m_models;
#[path = "../../../generated/modules/modules.rs"]
mod m_modules;
#[path = "../../../generated/modules/notes.rs"]
mod m_notes;
#[path = "../../../generated/modules/replacer.rs"]
mod m_replacer;
#[path = "../../../generated/modules/scheduler.rs"]
mod m_scheduler;
#[path = "../../../generated/modules/shell.rs"]
mod m_shell;
#[path = "../../../generated/modules/skills.rs"]
mod m_skills;
#[path = "../../../generated/modules/time.rs"]
mod m_time;
#[path = "../../../generated/modules/token_threshold.rs"]
mod m_token_threshold;
#[path = "../../../generated/modules/tutorial.rs"]
mod m_tutorial;
#[path = "../../../generated/modules/unsafe_shell.rs"]
mod m_unsafe_shell;
#[path = "../../../generated/modules/web_reader.rs"]
mod m_web_reader;
#[path = "../../../generated/modules/web_search.rs"]
mod m_web_search;
#[path = "../../../generated/modules/webui_extension_example.rs"]
mod m_webui_extension_example;
#[path = "../../../generated/modules/writing_style.rs"]
mod m_writing_style;

#[test]
fn tool_schemas_match_python() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("fixtures/python_tool_schemas.json")).unwrap();
    let fixture = fixture.as_object().unwrap();
    let mut count = 0;
    for spec in m_auto_backup::tools() {
        let name = format!("auto_backup_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_calculator::tools() {
        let name = format!("calculator_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_calendar::tools() {
        let name = format!("calendar_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_channel::tools() {
        let name = format!("channel_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_characters::tools() {
        let name = format!("characters_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_chats::tools() {
        let name = format!("chats_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_coder::tools() {
        let name = format!("coder_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_docs::tools() {
        let name = format!("docs_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_file_manager::tools() {
        let name = format!("file_manager_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_http::tools() {
        let name = format!("http_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_identity::tools() {
        let name = format!("identity_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_lists::tools() {
        let name = format!("lists_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_mcp::tools() {
        let name = format!("mcp_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_memory::tools() {
        let name = format!("memory_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_models::tools() {
        let name = format!("models_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_modules::tools() {
        let name = format!("modules_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_notes::tools() {
        let name = format!("notes_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_replacer::tools() {
        let name = format!("replacer_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_scheduler::tools() {
        let name = format!("scheduler_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_shell::tools() {
        let name = format!("shell_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_skills::tools() {
        let name = format!("skills_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_time::tools() {
        let name = format!("time_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_token_threshold::tools() {
        let name = format!("token_threshold_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_tutorial::tools() {
        let name = format!("tutorial_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_unsafe_shell::tools() {
        let name = format!("unsafe_shell_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_web_reader::tools() {
        let name = format!("web_reader_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_web_search::tools() {
        let name = format!("web_search_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_webui_extension_example::tools() {
        let name = format!("webui_extension_example_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    for spec in m_writing_style::tools() {
        let name = format!("writing_style_{}", spec.method);
        let ours = ol_core::tool_loader::tool_dict_from_spec(&spec, &name);
        let theirs = fixture.get(&name).unwrap_or_else(|| panic!("{} missing from python fixture", name));
        assert_eq!(ol_core::pyjson::dumps(&ours), ol_core::pyjson::dumps(theirs), "tool {} differs", name);
        count += 1;
    }
    assert_eq!(count, fixture.len());
}
