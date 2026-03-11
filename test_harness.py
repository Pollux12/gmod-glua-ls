#!/usr/bin/env python3
"""
Realistic LSP Performance Test Harness

Faithfully simulates VS Code's behavior: after each keystroke, the editor
sends didChange followed by multiple request types (inlayHint, semanticTokens,
completion, annotator, documentColor, etc.) and cancels previous pending requests.

This reproduces the "flood" pattern that causes the language server to freeze.
"""

import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

# =============================================================================
# LSP Protocol Helpers
# =============================================================================

class LspClient:
    """Minimal LSP client over stdio."""

    def __init__(self, process):
        self.process = process
        self.stdin = process.stdin
        self.stdout = process.stdout
        self._next_id = 1
        self._pending = {}
        self._lock = threading.Lock()
        self._reader_thread = threading.Thread(target=self._read_loop, daemon=True)
        self._reader_thread.start()
        self._notification_log = []
        self._notification_lock = threading.Lock()

    def _read_loop(self):
        buf = b""
        while True:
            try:
                chunk = self.stdout.read(1)
                if not chunk:
                    break
                buf += chunk
                if b"\r\n\r\n" in buf:
                    header_part, rest = buf.split(b"\r\n\r\n", 1)
                    headers = {}
                    for line in header_part.decode("utf-8").split("\r\n"):
                        if ":" in line:
                            k, v = line.split(":", 1)
                            headers[k.strip()] = v.strip()
                    content_length = int(headers.get("Content-Length", 0))
                    body = rest
                    while len(body) < content_length:
                        more = self.stdout.read(content_length - len(body))
                        if not more:
                            return
                        body += more
                    buf = body[content_length:]
                    json_body = body[:content_length]
                    try:
                        msg = json.loads(json_body.decode("utf-8"))
                    except json.JSONDecodeError:
                        continue
                    self._dispatch(msg)
            except Exception as e:
                print(f"[reader error] {e}", file=sys.stderr)
                break

    def _dispatch(self, msg):
        if "id" in msg and "method" in msg:
            self._handle_server_request(msg)
        elif "id" in msg:
            req_id = msg["id"]
            with self._lock:
                if req_id in self._pending:
                    entry = self._pending[req_id]
                    entry["result"] = msg
                    entry["event"].set()
        else:
            with self._notification_lock:
                self._notification_log.append(msg)

    def _handle_server_request(self, msg):
        method = msg.get("method", "")
        req_id = msg["id"]
        if method in ("workspace/diagnostic/refresh", "workspace/semanticTokens/refresh",
                       "workspace/inlayHint/refresh",
                       "window/workDoneProgress/create",
                       "client/registerCapability"):
            self._send_raw({"jsonrpc": "2.0", "id": req_id, "result": None})

    def _send_raw(self, msg):
        body = json.dumps(msg).encode("utf-8")
        header = f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8")
        try:
            self.stdin.write(header + body)
            self.stdin.flush()
        except Exception:
            pass

    def send_request(self, method, params, timeout=60.0):
        req_id = self._next_id
        self._next_id += 1
        event = threading.Event()
        entry = {"event": event, "result": None}
        with self._lock:
            self._pending[req_id] = entry
        self._send_raw({
            "jsonrpc": "2.0", "id": req_id,
            "method": method, "params": params,
        })
        start = time.monotonic()
        event.wait(timeout=timeout)
        elapsed = time.monotonic() - start
        with self._lock:
            self._pending.pop(req_id, None)
        return entry["result"], elapsed, req_id

    def send_request_nowait(self, method, params):
        """Fire a request and return its ID immediately (don't wait for response)."""
        req_id = self._next_id
        self._next_id += 1
        event = threading.Event()
        entry = {"event": event, "result": None}
        with self._lock:
            self._pending[req_id] = entry
        self._send_raw({
            "jsonrpc": "2.0", "id": req_id,
            "method": method, "params": params,
        })
        return req_id

    def send_notification(self, method, params):
        self._send_raw({"jsonrpc": "2.0", "method": method, "params": params})

    def send_cancel(self, req_id):
        self.send_notification("$/cancelRequest", {"id": req_id})

    def drain_request(self, req_id, timeout=0.001):
        """Wait very briefly for a request we don't care about."""
        with self._lock:
            entry = self._pending.get(req_id)
        if entry:
            entry["event"].wait(timeout=timeout)
            with self._lock:
                self._pending.pop(req_id, None)


def make_uri(file_path):
    path = Path(file_path).resolve().as_posix()
    return f"file:///{path}"


def initialize(client, workspace_path, glua_defs_path=None):
    workspace_folders = [{"uri": make_uri(workspace_path), "name": Path(workspace_path).name}]
    init_params = {
        "processId": os.getpid(),
        "clientInfo": {"name": "test-harness", "version": "1.0"},
        "rootUri": workspace_folders[0]["uri"],
        "workspaceFolders": workspace_folders,
        "capabilities": {
            "textDocument": {
                "synchronization": {"dynamicRegistration": True, "willSave": True,
                                     "willSaveWaitUntil": True, "didSave": True},
                "completion": {
                    "completionItem": {"snippetSupport": True, "commitCharactersSupport": True,
                                        "documentationFormat": ["markdown", "plaintext"],
                                        "deprecatedSupport": True, "preselectSupport": True,
                                        "labelDetailsSupport": True,
                                        "resolveSupport": {"properties": ["documentation", "detail"]}},
                    "contextSupport": True,
                },
                "hover": {"contentFormat": ["markdown", "plaintext"]},
                "signatureHelp": {
                    "signatureInformation": {"documentationFormat": ["markdown", "plaintext"],
                                              "parameterInformation": {"labelOffsetSupport": True}},
                },
                "diagnostic": {"dynamicRegistration": True},
                "semanticTokens": {
                    "dynamicRegistration": True, "requests": {"full": True},
                    "tokenTypes": ["namespace", "type", "class", "enum", "interface",
                                   "struct", "typeParameter", "parameter", "variable",
                                   "property", "enumMember", "event", "function",
                                   "method", "macro", "keyword", "modifier", "comment",
                                   "string", "number", "regexp", "operator", "decorator"],
                    "tokenModifiers": ["declaration", "definition", "readonly", "static",
                                       "deprecated", "abstract", "async", "modification",
                                       "documentation", "defaultLibrary"],
                    "formats": ["relative"], "multilineTokenSupport": True,
                },
                "inlayHint": {"dynamicRegistration": True},
                "publishDiagnostics": {"relatedInformation": True, "versionSupport": True,
                                        "tagSupport": {"valueSet": [1, 2]}},
                "codeAction": {"dynamicRegistration": True},
                "codeLens": {"dynamicRegistration": True},
                "foldingRange": {"dynamicRegistration": True},
                "documentSymbol": {"dynamicRegistration": True},
                "documentLink": {"dynamicRegistration": True},
                "colorProvider": {"dynamicRegistration": True},
            },
            "workspace": {
                "workspaceFolders": True, "configuration": True,
                "didChangeWatchedFiles": {"dynamicRegistration": True},
                "diagnostics": {"refreshSupport": True},
            },
            "window": {"workDoneProgress": True},
        },
        "initializationOptions": {},
    }
    result, elapsed, _ = client.send_request("initialize", init_params, timeout=120)
    print(f"  initialize: {elapsed:.2f}s")
    client.send_notification("initialized", {})
    print("  Waiting for workspace load...", end="", flush=True)
    time.sleep(8)
    print(" done")


def open_file(client, file_path, text=None):
    uri = make_uri(file_path)
    if text is None:
        with open(file_path, "r", encoding="utf-8", errors="replace") as f:
            text = f.read()
    client.send_notification("textDocument/didOpen", {
        "textDocument": {"uri": uri, "languageId": "glua", "version": 1, "text": text}
    })
    time.sleep(0.5)
    return uri, text


# =============================================================================
# Realistic VS Code flood test
# =============================================================================

def test_realistic_vscode_flood(client, workspace_path, typing_text, wpm=120):
    """
    Faithfully simulate VS Code behavior:
    - After EACH keystroke: didChange + cancel previous + fire off multiple requests
    - VS Code sends: inlayHint (often 2-3 ranges), semanticTokens/full,
      completion (with trigger), annotator, documentColor, etc.
    - After typing stops: measure time for final diagnostic/tokens to settle
    """
    print("\n" + "=" * 70)
    print("TEST: Realistic VS Code flood (multiple request types per keystroke)")
    print("=" * 70)

    lua_files = list(Path(workspace_path).rglob("*.lua"))
    if not lua_files:
        print("  ERROR: No Lua files found")
        return None

    test_file = lua_files[0]
    print(f"  File: {test_file}")

    uri, original_text = open_file(client, str(test_file))
    time.sleep(1)

    lines = original_text.split("\n")
    lines.append("")
    insert_line = len(lines) - 1
    insert_col = 0
    current_text = "\n".join(lines)
    version = 2

    client.send_notification("textDocument/didChange", {
        "textDocument": {"uri": uri, "version": version},
        "contentChanges": [{"text": current_text}],
    })
    version += 1
    time.sleep(0.5)

    chars_per_second = (wpm * 5) / 60
    delay = 1.0 / chars_per_second

    print(f"  Typing '{typing_text}' ({len(typing_text)} chars) at {wpm} WPM ({delay*1000:.0f}ms/char)")
    print(f"  Sending per keystroke: didChange + inlayHint(x2) + semanticTokens + annotator + completion + foldingRange + documentSymbol + codeLens + documentColor + documentLink + codeAction")
    pending_request_ids = []
    start_typing = time.monotonic()

    for i, char in enumerate(typing_text):
        col = insert_col + i
        line = lines[insert_line]
        new_line = line[:col] + char + line[col:]
        lines[insert_line] = new_line
        current_text = "\n".join(lines)

        # Cancel ALL previous requests (VS Code does this)
        for rid in pending_request_ids:
            client.send_cancel(rid)
        pending_request_ids.clear()

        # didChange notification
        client.send_notification("textDocument/didChange", {
            "textDocument": {"uri": uri, "version": version},
            "contentChanges": [{"text": current_text}],
        })
        version += 1

        # VS Code sends multiple request types after each keystroke:
        # 1. inlayHint (visible range - sometimes 2 for split viewport)
        rid = client.send_request_nowait("textDocument/inlayHint", {
            "textDocument": {"uri": uri},
            "range": {"start": {"line": 0, "character": 0},
                      "end": {"line": min(insert_line + 30, len(lines)), "character": 0}},
        })
        pending_request_ids.append(rid)

        # 2. Second inlayHint range (common in VS Code)
        rid = client.send_request_nowait("textDocument/inlayHint", {
            "textDocument": {"uri": uri},
            "range": {"start": {"line": max(0, insert_line - 10), "character": 0},
                      "end": {"line": min(insert_line + 50, len(lines)), "character": 0}},
        })
        pending_request_ids.append(rid)

        # 3. semanticTokens/full
        rid = client.send_request_nowait("textDocument/semanticTokens/full", {
            "textDocument": {"uri": uri},
        })
        pending_request_ids.append(rid)

        # 4. gluals/annotator (custom)
        rid = client.send_request_nowait("gluals/annotator", {
            "uri": str(uri),
        })
        pending_request_ids.append(rid)

        # 5. completion (every few keystrokes, like VS Code)
        if i % 3 == 0:
            rid = client.send_request_nowait("textDocument/completion", {
                "textDocument": {"uri": uri},
                "position": {"line": insert_line, "character": col + 1},
                "context": {"triggerKind": 1},
            })
            pending_request_ids.append(rid)

        # 6. Additional request types VS Code sends per keystroke
        rid = client.send_request_nowait("textDocument/foldingRange", {
            "textDocument": {"uri": uri},
        })
        pending_request_ids.append(rid)

        rid = client.send_request_nowait("textDocument/documentSymbol", {
            "textDocument": {"uri": uri},
        })
        pending_request_ids.append(rid)

        rid = client.send_request_nowait("textDocument/codeLens", {
            "textDocument": {"uri": uri},
        })
        pending_request_ids.append(rid)

        rid = client.send_request_nowait("textDocument/documentColor", {
            "textDocument": {"uri": uri},
        })
        pending_request_ids.append(rid)

        rid = client.send_request_nowait("textDocument/documentLink", {
            "textDocument": {"uri": uri},
        })
        pending_request_ids.append(rid)

        rid = client.send_request_nowait("textDocument/codeAction", {
            "textDocument": {"uri": uri},
            "range": {"start": {"line": insert_line, "character": 0},
                      "end": {"line": insert_line, "character": col + 1}},
            "context": {"diagnostics": []},
        })
        pending_request_ids.append(rid)

        time.sleep(delay)

    # Cancel last batch
    for rid in pending_request_ids:
        client.send_cancel(rid)
    pending_request_ids.clear()

    typing_done = time.monotonic()
    typing_time = typing_done - start_typing
    print(f"  Typing took: {typing_time:.2f}s")

    # Drain old request responses briefly
    time.sleep(0.05)

    # Now measure the settling time — what the user experiences after they stop typing
    # We want to measure TWO things:
    # 1. How quickly the LS responds at all (even with stale/empty data)
    # 2. How long until FRESH data is available (after reindex)
    print("\n  Post-typing settling:")

    # Measure immediate responsiveness (should be near-instant with is_dirty() bailout)
    t_start = time.monotonic()
    result, elapsed, _ = client.send_request("textDocument/semanticTokens/full", {
        "textDocument": {"uri": uri},
    }, timeout=30)
    has_tokens = result and result.get("result") is not None
    print(f"    semanticTokens/full (immediate): {elapsed*1000:.0f}ms [{'has data' if has_tokens else 'null/pending'}]")

    result, elapsed, _ = client.send_request("textDocument/inlayHint", {
        "textDocument": {"uri": uri},
        "range": {"start": {"line": 0, "character": 0},
                  "end": {"line": insert_line + 1, "character": 0}},
    }, timeout=30)
    has_hints = result and result.get("result") is not None
    print(f"    inlayHint (immediate): {elapsed*1000:.0f}ms [{'has data' if has_hints else 'null/pending'}]")

    immediate_time = time.monotonic() - t_start
    print(f"    Immediate response time: {immediate_time*1000:.0f}ms")

    # Now wait for fresh data — poll until we get non-null semantic tokens
    # This measures the debounce + reindex time
    print("\n    Waiting for fresh data (debounce + reindex)...")
    fresh_start = time.monotonic()
    max_wait = 10.0  # seconds
    got_fresh = False
    attempts = 0
    while time.monotonic() - fresh_start < max_wait:
        time.sleep(0.1)  # poll every 100ms
        attempts += 1
        result, elapsed, _ = client.send_request("textDocument/semanticTokens/full", {
            "textDocument": {"uri": uri},
        }, timeout=5)
        if result and result.get("result") is not None:
            got_fresh = True
            break

    fresh_time = time.monotonic() - fresh_start
    total_from_typing = time.monotonic() - typing_done
    if got_fresh:
        print(f"    Fresh data available after: {fresh_time*1000:.0f}ms ({attempts} polls)")
    else:
        print(f"    ⚠ No fresh data after {max_wait}s ({attempts} polls)")

    # Now get diagnostics (these wait for reindex)
    result, elapsed, _ = client.send_request("textDocument/diagnostic", {
        "textDocument": {"uri": uri},
    }, timeout=30)
    status = "OK" if result and "result" in result else "TIMEOUT" if result is None else "ERR"
    print(f"    diagnostic: {elapsed*1000:.0f}ms [{status}]")

    total = time.monotonic() - t_start
    print(f"\n  Total from typing-end to fresh data: {total_from_typing:.2f}s")

    # Also test copy-paste baseline for comparison
    print("\n  --- BASELINE: Copy-paste same text (single didChange) ---")
    # Reset to original
    client.send_notification("textDocument/didChange", {
        "textDocument": {"uri": uri, "version": version},
        "contentChanges": [{"text": original_text}],
    })
    version += 1
    time.sleep(2)

    paste_text = original_text + "\n" + typing_text
    paste_start = time.monotonic()
    client.send_notification("textDocument/didChange", {
        "textDocument": {"uri": uri, "version": version},
        "contentChanges": [{"text": paste_text}],
    })
    version += 1
    time.sleep(0.05)

    t1 = time.monotonic()
    result, elapsed, _ = client.send_request("textDocument/semanticTokens/full", {
        "textDocument": {"uri": uri},
    }, timeout=30)
    print(f"    semanticTokens/full: {elapsed*1000:.0f}ms")

    result, elapsed, _ = client.send_request("textDocument/diagnostic", {
        "textDocument": {"uri": uri},
    }, timeout=30)
    print(f"    diagnostic: {elapsed*1000:.0f}ms")

    paste_total = time.monotonic() - paste_start
    print(f"  Total from paste to last response: {paste_total:.2f}s")

    return total_from_typing


# =============================================================================
# Main
# =============================================================================

def main():
    import argparse
    parser = argparse.ArgumentParser(description="Realistic LSP Performance Test")
    parser.add_argument("--typing-text", default="aswdoawkdawklfsekljgsdlkadwaijdds",
                        help="Text to type (default: 33 chars)")
    parser.add_argument("--wpm", type=int, default=120, help="Words per minute")
    parser.add_argument("--no-glua-defs", action="store_true")
    parser.add_argument("--runs", type=int, default=1, help="Number of test runs")
    args = parser.parse_args()

    base_dir = Path(__file__).parent
    ls_binary = base_dir / "target" / "release" / "glua_ls.exe"
    workspace_path = base_dir / "test_addon"
    glua_defs = Path(r"C:\Users\Pollux\Documents\glualangserver\emmylua-rust\glua-api-snippets\output")

    if not ls_binary.exists():
        print(f"ERROR: LS binary not found at {ls_binary}")
        sys.exit(1)
    if not workspace_path.exists():
        print(f"ERROR: Workspace not found at {workspace_path}")
        sys.exit(1)

    # Create .gluarc.json
    gluarc_path = workspace_path / ".gluarc.json"
    gluarc = {"$schema": "https://...", "gmod": {"enabled": True}}
    if not args.no_glua_defs and glua_defs.exists():
        gluarc["workspace"] = {"library": [str(glua_defs).replace("\\", "/")]}
    with open(gluarc_path, "w") as f:
        json.dump(gluarc, f, indent=2)

    print(f"LS binary: {ls_binary}")
    print(f"Workspace: {workspace_path}")
    print(f"GLua defs: {glua_defs if not args.no_glua_defs else 'disabled'}")
    print(f"Typing: '{args.typing_text}' ({len(args.typing_text)} chars) at {args.wpm} WPM")
    print()

    results = []
    for run_num in range(1, args.runs + 1):
        if args.runs > 1:
            print(f"\n{'#'*70}")
            print(f"# RUN {run_num}/{args.runs}")
            print(f"{'#'*70}")

        log_path = base_dir / "test_ls.log"
        process = subprocess.Popen(
            [str(ls_binary), "--log-level", "info", "--log-path", str(log_path)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )

        try:
            client = LspClient(process)
            print("Starting language server...")
            print("Initializing...")
            initialize(client, str(workspace_path))

            settle_time = test_realistic_vscode_flood(
                client, str(workspace_path), args.typing_text, wpm=args.wpm
            )
            if settle_time is not None:
                results.append(settle_time)

            print("\n" + "=" * 70)
            print("DONE")
            print("=" * 70)

            client.send_request("shutdown", None, timeout=10)
            client.send_notification("exit", None)
            time.sleep(1)
        finally:
            process.kill()
            process.wait()
            if gluarc_path.exists():
                os.remove(gluarc_path)

    if results:
        print(f"\n\nSUMMARY ({len(results)} runs):")
        print(f"  Settle times: {[f'{t:.2f}s' for t in results]}")
        print(f"  Average: {sum(results)/len(results):.2f}s")
        print(f"  Max: {max(results):.2f}s")
        if max(results) > 2.0:
            print(f"  ❌ FAIL: Max settle time {max(results):.2f}s > 2.0s threshold")
        else:
            print(f"  ✅ PASS: All settle times under 2.0s")


if __name__ == "__main__":
    main()
