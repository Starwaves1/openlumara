import core
import channels.webui.api as webui
import os
import msgpack
import datetime
import re
import ulid

class Memory(core.module.Module):
    """Gives your AI a persistent memory system"""

    category = "core"

    settings = {
        "insert_nudge_prompt": {
            "default": False,
            "description": "Whether to put extra instructions in the end prompt (after message history) to help the AI autonomously use its memory system, so that it remembers things without you having to explicitely ask it to."
        },
        "put_pinned_memories_in_system_prompt": {
            "description": "Whether to put the AI's pinned memories in the system prompt. Pinned memories are memories that you or the AI has decided it needs to remember at all times. You can pin a memory by asking your AI to pin it, or sometimes it'll decide to pin by itself if it has decided a memory is important enough. You can always ask it to unpin a memory you don't want pinned!",
            "default": True
        },
        "max_pinned_memories": {
            "default": 20,
            "depends": "put_pinned_memories_in_system_prompt"
        }
    }

    async def on_ready(self):
        self._mem = core.storage.StorageList("memory", type="msgpack")
        self._mem_deleted = core.storage.StorageList("deleted_memories", type="json")

    def _get_index(self, ulid: str) -> int:
        """checks if a memory with ID exists in memories"""
        for index, mem in enumerate(self._mem):
            if ulid.strip() == mem.get("id").strip():
                return index
        return -1

    async def on_system_prompt(self):
        pinned_str = ""
        automem_prompt = ""

        if self.config.get("put_pinned_memories_in_system_prompt"):
            count = 1
            prompt_mem_list = []
            for mem in reversed(self._mem):
                if not mem.get("pinned"):
                    continue

                if count > self.config.get("max_pinned_memories"):
                    break

                prompt_mem_list.append(mem)
                count += 1

            pinned = [f"{m['id']}:\n{m['content']}" for m in prompt_mem_list]
            pinned_str = "\n\n".join(pinned) or "There are currently no pinned memories."
            pinned_str+="\n\n"

        if not pinned_str and not automem_prompt:
            return None

        return f"{pinned_str}{automem_prompt}"

    async def on_end_prompt(self):
        automem_prompt = None
        if self.config.get("insert_nudge_prompt"):
            automem_prompt = "You are responsible for managing your own long-term memory. You must proactively and autonomously decide to use the memory tools to maintain an accurate, up-to-date, and efficient record of the user, your own operational preferences, and important contextual facts. Do not wait for instructions to remember; if information is valuable for future interactions, store it immediately."

        return automem_prompt or None

    @core.module.tool
    async def create(self, content: str, tags: list, pinned: bool = False):
        """Creates a new persistent memory. Use for storing relevant info, preferences, or context for future interactions.
        
        Args:
            content: the contents of the memory
            tags: a list of tags to associate with the memory for later lookup
            pinned: whether to pin a memory to the top of your context window (use for high-importance facts)
        """
        mem = {
            "id": str(ulid.ULID()),
            "content": content,
            "tags": tags,
            "pinned": pinned,
            "date_created": datetime.datetime.now().isoformat()
        }
        self._mem.append(mem)
        self._mem.save()
        return self.result(f"memory added. ID: {mem['id']}")

    @core.module.tool
    async def edit(self, id: str, content: str = None, tags: list = None):
        """Edits an existing memory. Use for self-maintenance/updating outdated info. CAUTION: ONLY use if you can see the memory's ID; NEVER hallucinate IDs."""
        index = self._get_index(id)
        if index == -1:
            return self.result("memory with that ID not found!")

        if content:
            self._mem[index]["content"] = content
        if tags:
            self._mem[index]["tags"] = tags

        self._mem.save()
        return self.result(f"memory {id} edited")

    @core.module.tool
    async def delete(self, id: str):
        """Deletes a memory. Use to prune irrelevant/redundant info. DANGEROUS: Ensure memory is truly obsolete before deleting."""
        index = self._get_index(id)
        if index == -1:
            return self.result("memory with that ID not found!")

        self._mem_deleted.append(self._mem[index])
        self._mem_deleted.save()

        self._mem.pop(index)
        self._mem.save()
        return self.result(f"memory {id} deleted")

    @core.module.tool
    async def pin(self, id: str):
        """Pins a memory to the top of your active context window. Use for critical identity, user preferences, or high-priority goals."""
        index = self._get_index(id)
        if index == -1:
            return self.result("memory with that ID not found!")

        self._mem[index]["pinned"] = True
        self._mem.save()
        return self.result(f"memory {id} pinned")

    @core.module.tool
    async def unpin(self, id: str):
        """Unpins a memory to manage cognitive load. Use when a pinned memory is no longer high priority."""
        index = self._get_index(id)
        if index == -1:
            return self.result("memory with that ID not found!")

        self._mem[index]["pinned"] = False
        self._mem.save()
        return self.result(f"memory {id} unpinned")

    @core.module.tool
    async def search(self, query: str, search_in_content: bool = True):
        """Searches memories by query. Use when you need to recall past info but don't know the exact ID."""
        weights = {"tags": 2.0, "content": 1.0} if search_in_content else {"tags": 2.0}

        results = await self._mem.search(query, field_weights=weights, top_n=10)

        if not results:
            return self.result(f"No memories found matching '{query}'.")

        lines = []
        for hit in results:
            mem = hit["entry"]
            lines.append(
                f"ID: {mem.get('id')} | Score: {hit['score']:.2f} | Tags: {mem.get('tags')} | Content: {mem.get('content')}"
            )

        return self.result("\n".join(lines))

    @core.module.tool
    async def list_unpinned(self, tag: str = None):
        """Lists all unpinned memories. Use to browse long-term storage or filter by tag."""
        results = []
        for mem in self._mem:
            if not mem.get("pinned"):
                tags = mem.get("tags", [])

                if tag:
                    # Filter by tag (category)
                    if any(tag.lower() in t.lower() for t in tags):
                        results.append(f"ID: {mem.get('id')} | Tags: {tags} | Content: {mem.get('content')}")
                else:
                    # List everything unpinned
                    results.append(f"ID: {mem.get('id')} | Tags: {tags} | Content: {mem.get('content')}")

        if not results:
            msg = "No unpinned memories found."
            if tag:
                msg += f" (No unpinned memories found with tag '{tag}')"
            return self.result(msg)

        return self.result("\n".join(results))

    # -- AI GENERATED CODE (Qwen3.8 Flash Next) :: (2026-10-13)
    # webui extension routes for the memory sidebar panel. Private methods are
    # ignored by the tool loader; @webui.route exposes them under
    # /api/ext/memory/<path>.
    def _ui_item(self, mem, score=None):
        """compact JSON shape used by the webui memory sidebar"""
        item = {
            "id": mem.get("id"),
            "content": mem.get("content", ""),
            "tags": mem.get("tags", []) or [],
            "pinned": bool(mem.get("pinned")),
            "date_created": mem.get("date_created", ""),
        }
        if score is not None:
            item["score"] = round(float(score), 2)
        return item

    def _matches_tag(self, mem, tag):
        """case-insensitive tag filter used by the webui routes"""
        if not tag:
            return True

        wanted = str(tag).strip().lower()
        if not wanted:
            return True

        for mem_tag in mem.get("tags", []) or []:
            if str(mem_tag).strip().lower() == wanted:
                return True

        return False

    @webui.route("tags")
    async def _route_tags(self, body=None, query=None):
        """unique memory tags for the current sidebar context"""
        self._mem.load()

        tab = str((query or {}).get("tab", "all")).lower()
        q = str((query or {}).get("q", "")).strip()

        limit = 200
        try:
            limit = int((query or {}).get("limit", 200) or 200)
        except ValueError:
            limit = 200

        limit = max(1, min(limit, 500))

        if tab == "pinned":
            entries = [mem for mem in self._mem if mem.get("pinned")]
        elif tab == "search" and q:
            search_in_content = str((query or {}).get("search_content", "true")).lower() not in ("false", "0", "no")
            weights = {"tags": 2.0, "content": 1.0} if search_in_content else {"tags": 2.0}

            results = await core.search.search(list(self._mem), q, field_weights=weights, top_n=limit)
            entries = [hit.get("entry", {}) for hit in results if hit.get("entry")]
        else:
            entries = list(self._mem)

        tags = set()
        for mem in entries:
            for tag in mem.get("tags", []) or []:
                tag = str(tag).strip()
                if tag:
                    tags.add(tag)

        return sorted(tags, key=str.lower)

    @webui.route("list")
    async def _route_list(self, body=None, query=None):
        """newest-first memories, optionally filtered by tag"""
        self._mem.load()

        limit = 100
        try:
            limit = int((query or {}).get("limit", 100) or 100)
        except ValueError:
            limit = 100

        limit = max(1, min(limit, 500))

        tag = (query or {}).get("tag", "")
        items = []
        for mem in reversed(self._mem):
            if not self._matches_tag(mem, tag):
                continue

            items.append(self._ui_item(mem))
            if len(items) >= limit:
                break

        return items

    @webui.route("list_pinned")
    async def _route_list_pinned(self, body=None, query=None):
        """newest-first pinned memories for the sidebar"""
        self._mem.load()

        limit = 0
        try:
            limit = int((query or {}).get("limit", 0) or 0)
        except ValueError:
            limit = 0

        tag = (query or {}).get("tag", "")
        items = []
        for mem in reversed(self._mem):
            if not mem.get("pinned") or not self._matches_tag(mem, tag):
                continue

            items.append(self._ui_item(mem))
            if limit and len(items) >= limit:
                break

        return items

    @webui.route("search")
    async def _route_search(self, body=None, query=None):
        """ranked memory search for the sidebar. Returns raw JSON entries."""
        self._mem.load()

        q = str((query or {}).get("q", "")).strip()
        if not q:
            return []

        limit = 20
        try:
            limit = int((query or {}).get("limit", 20) or 20)
        except ValueError:
            limit = 20

        limit = max(1, min(limit, 100))

        tag = (query or {}).get("tag", "")
        entries = [mem for mem in self._mem if self._matches_tag(mem, tag)]
        if not entries:
            return []

        search_in_content = str((query or {}).get("search_content", "true")).lower() not in ("false", "0", "no")
        weights = {"tags": 2.0, "content": 1.0} if search_in_content else {"tags": 2.0}

        results = await core.search.search(entries, q, field_weights=weights, top_n=limit)
        return [self._ui_item(hit.get("entry", {}), hit.get("score")) for hit in results if hit.get("entry")]

    @webui.route("pin", method="POST")
    async def _route_pin(self, body=None, query=None):
        mem_id = (body or {}).get("id")
        if not mem_id:
            return self.result("no memory id given", success=False)

        self._mem.load()
        return await self.pin(mem_id)

    @webui.route("unpin", method="POST")
    async def _route_unpin(self, body=None, query=None):
        mem_id = (body or {}).get("id")
        if not mem_id:
            return self.result("no memory id given", success=False)

        self._mem.load()
        return await self.unpin(mem_id)

    @webui.route("update", method="POST")
    async def _route_update(self, body=None, query=None):
        mem_id = (body or {}).get("id")
        if not mem_id:
            return self.result("no memory id given", success=False)

        self._mem.load()

        content = (body or {}).get("content")
        if content is None or not str(content).strip():
            return self.result("memory content cannot be empty", success=False)

        tags = (body or {}).get("tags", [])
        if not isinstance(tags, list):
            tags = []

        index = self._get_index(mem_id)
        if index == -1:
            return self.result("memory with that ID not found!", success=False)

        self._mem[index]["content"] = str(content).strip()
        self._mem[index]["tags"] = [str(tag).strip() for tag in tags if str(tag).strip()]
        self._mem.save()

        return self.result(f"memory {mem_id} edited")

    @webui.route("delete", method="POST")
    async def _route_delete(self, body=None, query=None):
        mem_id = (body or {}).get("id")
        if not mem_id:
            return self.result("no memory id given", success=False)

        self._mem.load()
        return await self.delete(mem_id)
