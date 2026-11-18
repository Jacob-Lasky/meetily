"""Entry point: `uv run --project <dir> python -m meetily_sidecar`.

Handshake with the parent (Meetily):
1. stdin, first line: `{"token": "...", "deepgram_api_key": "..."}`. Secrets
   come this way, never on argv or in the environment, where `ps` and
   `/proc/<pid>/environ` would show them.
2. stdout, one line: `{"port": N}` once the server listens on 127.0.0.1:N.
   Nothing else is ever written to stdout: fd 1 is pointed at stderr before
   anything (Pipecat, loguru, a stray print) can write to it.
3. stdin EOF means the parent is gone (closed or crashed): the sidecar exits,
   so it never outlives the recording.
"""

from __future__ import annotations

import asyncio
import json
import os
import sys


def _take_stdout() -> int:
    """Keep a private copy of the real stdout and send fd 1 to stderr."""
    handshake_fd = os.dup(1)
    os.dup2(2, 1)
    sys.stdout = sys.stderr
    return handshake_fd


async def _main(handshake_fd: int) -> int:
    from loguru import logger

    from .server import Sidecar, serve_forever

    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader()
    await loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
    line = await reader.readline()
    try:
        boot = json.loads(line)
        token, key = str(boot["token"]), str(boot["deepgram_api_key"])
    except (ValueError, KeyError, TypeError):
        logger.error("bad handshake on stdin; expected {token, deepgram_api_key}")
        return 2
    if len(token) < 32:
        logger.error("refusing a run token shorter than 32 characters")
        return 2

    stop = asyncio.Event()

    async def watch_parent():
        while await reader.read(4096):
            pass
        logger.info("parent closed stdin; shutting down")
        stop.set()

    def on_ready(port: int):
        os.write(handshake_fd, (json.dumps({"port": port}) + "\n").encode())
        os.close(handshake_fd)
        logger.info(f"listening on 127.0.0.1:{port}")

    watcher = asyncio.create_task(watch_parent())
    try:
        await serve_forever(Sidecar(token=token, deepgram_api_key=key), stop, on_ready)
    finally:
        watcher.cancel()
    return 0


#: Loggers whose DEBUG lines carry MCP tool results (Pipecat's MCPClient logs
#: each response chunk): a lookup's results are meeting-derived and private,
#: and these lines land in Meetily's log, so they are dropped even when
#: MEETILY_SIDECAR_LOG=DEBUG.
PRIVATE_DEBUG_LOGGERS = ("pipecat.services.mcp_service",)


def keep_record(record) -> bool:
    name = record["name"] or ""
    return not (record["level"].no < 20 and name.startswith(PRIVATE_DEBUG_LOGGERS))


def main() -> int:
    handshake_fd = _take_stdout()
    from loguru import logger

    logger.remove()
    logger.add(sys.stderr, level=os.environ.get("MEETILY_SIDECAR_LOG", "INFO"), filter=keep_record)
    return asyncio.run(_main(handshake_fd))


if __name__ == "__main__":
    sys.exit(main())
