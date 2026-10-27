# AI Written: Claude Opus 5.5
import asyncio
from types import SimpleNamespace

import pytest

import core


@pytest.fixture
def api(manager):
    return manager.API


async def history(channel):
    return [(m["role"], m["content"]) for m in await channel.context.chat.messages.get()]


async def test_send_stores_user_and_assistant(channel, api):
    api.replies.append({"role": "assistant", "content": "hi there"})

    reply = await channel.send("hello")

    assert reply["content"] == "hi there"
    assert api.sent[0]["context"][-1] == {"role": "user", "content": "hello"}
    assert await history(channel) == [("user", "hello"), ("assistant", "hi there")]


@pytest.mark.parametrize("message, authorized, expected", [
    ("/ping", True, "pong"),
    ("/ping", False, "not authorized"),
])
async def test_commands_skip_the_api(channel, api, message, authorized, expected):
    reply = await channel.send(message, commands_authorized=authorized)
    assert expected in reply["content"]
    assert api.sent == []


def hook(fn, is_async):
    if not is_async:
        return SimpleNamespace(on_user_message=fn)

    async def on_user_message(message):
        return fn(message)
    return SimpleNamespace(on_user_message=on_user_message)


@pytest.mark.parametrize("is_async", [False, True])
async def test_user_message_hook_can_rewrite(channel, manager, api, is_async):
    manager.modules["shout"] = hook(str.upper, is_async)
    api.replies.append({"role": "assistant", "content": "ok"})

    await channel.send("hello")

    assert api.sent[0]["context"][-1]["content"] == "HELLO"
    assert (await history(channel))[0] == ("user", "HELLO")


async def test_user_message_hook_can_intercept(channel, manager, api):
    manager.modules["eater"] = hook(lambda message: False, is_async=False)

    assert await channel.send("hello") is None
    assert api.sent == []
    assert await history(channel) == [("user", "hello")]


async def test_assistant_message_hook_sees_reply(channel, manager, api):
    seen = []
    manager.modules["spy"] = SimpleNamespace(on_assistant_message=seen.append)
    api.replies.append({"role": "assistant", "content": "hi"})

    await channel.send("hello")
    assert seen == ["hi"]


async def test_api_error_becomes_assistant_message(channel, api):
    api.replies.append(core.api.APIError("Rate limit exceeded"))
    reply = await channel.send("hello")
    assert reply == {"role": "assistant", "content": "Rate limit exceeded"}


async def test_connect_failure_skips_the_request(channel, api):
    async def fail():
        return core.api.APIError("Failed to connect to the API")
    api.attempt_connect = fail

    reply = await channel.send("hello")
    assert "Failed to connect" in reply["content"]
    assert api.sent == []


async def test_stream_assembles_final_message(channel, api):
    api.replies.append([
        {"type": "reasoning", "content": "let me "},
        {"type": "reasoning", "content": "think"},
        {"type": "content", "content": "Hel"},
        {"type": "content", "content": "lo"},
    ])

    tokens = [t async for t in channel.send_stream("hi")]

    assert tokens[0] == {"type": "user_message", "content": "hi"}
    assert tokens[-1]["type"] == "token_usage"
    last = (await channel.context.chat.messages.get())[-1]
    assert (last["role"], last["content"], last["reasoning_content"]) == ("assistant", "Hello", "let me think")


async def test_stream_error_keeps_partial_reply(channel, api):
    api.replies.append([
        {"type": "content", "content": "partial"},
        {"type": "error", "content": "connection lost"},
    ])

    tokens = [t async for t in channel.send_stream("hi")]

    assert tokens[-1] == {"type": "error", "content": "connection lost"}
    assert (await history(channel))[1:] == [("assistant", "partial"), ("assistant", "Error: connection lost")]


async def test_stream_command_yields_reply_without_api(channel, api):
    tokens = [t async for t in channel.send_stream("/ping", commands_authorized=True)]
    assert [t["type"] for t in tokens] == ["user_message", "content"]
    assert "pong" in tokens[1]["content"]
    assert api.sent == []


async def test_deferred_push_lands_after_the_reply(channel, api):
    await channel._start_push_queue()
    send = api.send

    async def send_and_push(context, **kwargs):
        # a module pushing mid-turn, e.g. from a tool
        await channel.push("deferred note")
        await channel.push("immediate note", defer=False)
        await asyncio.sleep(0.01)
        return await send(context, **kwargs)
    api.send = send_and_push
    api.replies.append({"role": "assistant", "content": "reply"})

    await channel.send("hello")
    await asyncio.wait_for(channel.push_queue.join(), timeout=1)

    assert await history(channel) == [
        ("user", "hello"),
        ("assistant", "immediate note"),
        ("assistant", "reply"),
        ("assistant", "deferred note"),
    ]
    assert sorted(m["content"] for m in channel.pushed) == ["deferred note", "immediate note"]
