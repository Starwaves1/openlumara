# AI Written: Claude Opus 5.5
"""
Bugs found while writing this suite, one strict xfail each.
When one gets fixed its test starts failing (XPASS): drop the marker and move it next to its neighbours.
"""

import json
import os

import pytest

import core

bug = pytest.mark.xfail(strict=True)


@bug
def test_yaml_storage_list_can_save(tmp_path):
    # core/storage.py StorageList.save dumps the list subclass itself, which yaml.safe_dump can't represent
    items = core.storage.StorageList("things", "yaml", path=str(tmp_path))
    items.append("a")
    items.save()


@bug
def test_emptied_text_storage_list_is_saved(tmp_path):
    # core/storage.py StorageList.save skips the write for an empty text list
    lines = core.storage.StorageList("lines", "text", path=str(tmp_path))
    lines.append("stale")
    lines.save()
    lines.clear()
    lines.save()

    assert list(core.storage.StorageList("lines", "text", path=str(tmp_path))) == []


@bug
def test_rejected_markdown_key_does_not_break_later_saves(tmp_path):
    # core/storage.py _delete_nested_key splits "../escape" on "/", so the bad key is never removed
    notes = core.storage.StorageDict("notes", "markdown", path=str(tmp_path))
    notes["../escape"] = "x"
    with pytest.raises(ValueError):
        notes.save()

    notes["fine"] = "y"
    notes.save()


@bug
async def test_cleared_chat_stays_cleared(channel):
    # core/messages.py Messages.clear never saves the history file
    await channel.context.chat.messages.add({"role": "user", "content": "hi"})
    await channel.context.chat.clear()

    with open(channel.context.chat.messages.data.path) as f:
        assert json.load(f) == []


@bug
async def test_image_only_message_keeps_default_title(channel):
    # core/messages.py: _extract_content returns "" for media, and "" passes the isinstance(str) check
    await channel.context.chat.messages.add(
        {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "data:,"}}]}
    )
    assert channel.context.chat.get("title") == "New chat"


@bug
async def test_null_content_user_message_is_kept(channel):
    # core/context.py Context.get: the `continue` skips appending non-assistant messages with content None
    channel.context.chat.messages.data.append({"role": "user", "content": None})
    assert await channel.context.get() == [{"role": "user", "content": ""}]


@bug
async def test_failing_user_message_hook_does_not_break_send(channel, manager):
    # core/channel.py: usr_msg_result is unbound when the first on_user_message hook raises
    class Broken:
        async def on_user_message(self, message):
            raise RuntimeError("boom")

    manager.modules["broken"] = Broken()
    manager.API.replies.append({"role": "assistant", "content": "hello"})
    await channel.send("hi")


@bug
async def test_history_full_shows_system_prompt(channel, monkeypatch):
    # core/channel.py format_message only renders user/assistant/tool, so the system message comes out blank
    async def prompt(*args, **kwargs):
        return "be nice"
    monkeypatch.setattr(channel.context.chat, "get_system_prompt", prompt)
    await channel.context.chat.messages.add({"role": "user", "content": "hi"})

    output = await channel.commands.cmd_history(["full"])
    assert "be nice" in output


@bug
def test_sandbox_path_strips_embedded_absolute_base(tmp_path):
    # core/functions.py sandbox_path strips the leading "/" before comparing against base + "/"
    base = str(tmp_path)
    assert core.sandbox_path(base, f"{base}/file.txt") == f"{base}/file.txt"


@bug
def test_symlinked_directory_raises_value_error(tmp_path):
    # core/functions.py sandbox_open only translates ELOOP; a symlinked dir gives ENOTDIR
    (tmp_path / "base").mkdir()
    (tmp_path / "outside").mkdir()
    (tmp_path / "outside" / "secret.txt").write_text("x")
    os.symlink(tmp_path / "outside", tmp_path / "base" / "link")

    with pytest.raises(ValueError):
        core.sandbox_open(str(tmp_path / "base"), "link/secret.txt", os.O_RDONLY)


@bug
async def test_scheduler_contains_a_failing_job(manager, tmp_path):
    # modules/scheduler: the error handlers call self.log_error, which Module doesn't have
    from modules.scheduler.module import Scheduler

    sched = Scheduler(manager)
    sched.schedule = core.storage.StorageList("schedule", "json", path=str(tmp_path))
    sched.schedule.append({"id": "j1", "recurring": False})

    async def boom(job):
        raise RuntimeError("job failed")
    sched._execute_job = boom

    await sched._job_wrapper(sched.schedule[0])
