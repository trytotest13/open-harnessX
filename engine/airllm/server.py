# AirLLM sidecar for Open Harness.
#
# Wraps the layer-streaming AirLLM engine (research/airllm — streams one
# transformer layer at a time from disk) behind the same OpenAI-compatible
# wire surface llama-server exposes on port 8788, so the Rust supervisor
# can start either engine without changing anything downstream:
#
#   GET  /health                 200 once the model is loaded and serving
#   GET  /props                  llama-server shape (vision: false)
#   GET  /v1/models              single-entry model list, alias = folder name
#   POST /v1/chat/completions    SSE streaming or plain JSON
#
# Usage (spawned by open-harness.exe, or by hand for debugging):
#   python server.py --model "D:\Ai Ollama\DeepSeek-R1" --host 127.0.0.1 \
#       --port 8788 --max-seq-len 16384 --compression 4bit
#
# Honest trade-off, same as AirLLM itself: memory first, not tokens/sec.
# Layers stream from disk every forward pass, so decode speed is far below
# llama.cpp; use this mode for checkpoints that cannot fit in RAM/VRAM.

import argparse
import json
import queue
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO_AIRLLM = Path(__file__).resolve().parents[2] / "research" / "airllm" / "air_llm"
if REPO_AIRLLM.is_dir():
    # Use the vendored checkout instead of a pip-installed airllm; the repo
    # also keeps model-family support newer than the PyPI release.
    sys.path.insert(0, str(REPO_AIRLLM))

from airllm import AutoModel  # noqa: E402

ARGS = None
MODEL = None
STATE = {"loaded": False, "error": None, "loading": False}


def alias() -> str:
    return Path(ARGS.model).name or "model"


def load_model():
    STATE["loading"] = True
    try:
        print(f"[airllm] loading {ARGS.model} (compression={ARGS.compression or 'none'}); "
              f"first load splits layers to disk and can take a long while", flush=True)
        kwargs = {
            "compression": ARGS.compression or None,
            "max_seq_len": ARGS.max_seq_len,
        }
        if ARGS.shards_dir:
            kwargs["layer_shards_saving_path"] = ARGS.shards_dir
        model = AutoModel.from_pretrained(ARGS.model, **kwargs)
        globals()["MODEL"] = model
        STATE["loaded"] = True
        print(f"[airllm] ready: {alias()}", flush=True)
    except Exception as e:  # noqa: BLE001 - report any load failure over /health
        STATE["error"] = str(e)
        print(f"[airllm] load failed: {e}", flush=True)
        raise
    finally:
        STATE["loading"] = False


def chat_tokens(messages):
    try:
        prompt = MODEL.tokenizer.apply_chat_template(
            messages, tokenize=False, add_generation_prompt=True)
    except Exception:
        # Checkpoint ships no chat template — fall back to plain text so the
        # engine still answers instead of 400-ing every request.
        prompt = "\n".join(f"{m.get('role', 'user')}: {m.get('content', '')}"
                           for m in messages) + "\nassistant:"
    enc = MODEL.tokenizer([prompt], return_tensors="pt",
                          return_attention_mask=False, truncation=True,
                          max_length=ARGS.max_seq_len)
    return enc["input_ids"]


def generate_tokens(input_ids, sample_kwargs, sink):
    """Run AirLLM generate on a worker thread.

    Tries token-level streaming through an HF TextIteratorStreamer (AirLLM's
    generate forwards to a GenerationMixin-compatible call). Some model
    families reject the streamer kwarg — detected by polling, never by
    iterating the streamer (its stop signal never arrives on a rejected
    generate, so a plain for-loop would deadlock) — and those retry without
    streaming, pushing the full completion as one chunk.
    """
    out_q = queue.Queue()

    def run(with_streamer, streamer=None):
        try:
            kwargs = dict(sample_kwargs)
            if with_streamer and streamer is not None:
                kwargs["streamer"] = streamer
            gen = MODEL.generate(input_ids.cuda(),
                                 max_new_tokens=ARGS.max_new_tokens,
                                 use_cache=True,
                                 return_dict_in_generate=True,
                                 **kwargs)
            out_q.put(("ok", gen))
        except Exception as e:  # noqa: BLE001
            out_q.put(("err", e))

    streamer = None
    try:
        from transformers import TextIteratorStreamer
        streamer = TextIteratorStreamer(MODEL.tokenizer, skip_prompt=True,
                                        skip_special_tokens=True)
    except Exception:
        pass

    if streamer is None:
        threading.Thread(target=run, args=(False,), daemon=True).start()
        _, gen = out_q.get()
        sink(decode_new_tokens(gen, input_ids.shape[1]))
        return

    worker = threading.Thread(target=run, args=(True, streamer), daemon=True)
    worker.start()
    stop_signal = getattr(streamer, "stop_signal", None)
    while True:
        try:
            piece = streamer.text_queue.get(timeout=0.5)
        except queue.Empty:
            if not worker.is_alive():
                break  # generate died without end()ing the streamer
            continue
        if piece is stop_signal:
            break
        if piece:
            sink(piece)
    status, payload = out_q.get()
    if status == "ok":
        return
    if isinstance(payload, Exception) and "streamer" in str(payload):
        worker = threading.Thread(target=run, args=(False,), daemon=True)
        worker.start()
        _, gen = out_q.get()
        sink(decode_new_tokens(gen, input_ids.shape[1]))
        return
    raise payload


def decode_new_tokens(gen, prompt_len):
    return MODEL.tokenizer.decode(gen.sequences[0][prompt_len:],
                                  skip_special_tokens=True)


def sse_chunk(chunk_id, model, delta=None, finish=None):
    obj = {
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": model,
        "choices": [{"index": 0, "delta": delta or {},
                     "finish_reason": finish}],
    }
    return f"data: {json.dumps(obj)}\n\n"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        print(f"[airllm] {self.command} {self.path}", flush=True)

    def _send(self, code, body, ctype="application/json"):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path == "/health":
            if STATE["error"]:
                self._send(503, json.dumps({"error": STATE["error"]}))
            elif STATE["loaded"]:
                self._send(200, json.dumps({"status": "ok", "alias": alias()}))
            else:
                self._send(503, json.dumps({"status": "loading"}))
        elif path == "/props":
            self._send(200, json.dumps({
                "modalities": {"vision": False},
                "model_path": ARGS.model,
                "airllm": True,
            }))
        elif path == "/v1/models":
            self._send(200, json.dumps({
                "object": "list",
                "data": [{"id": alias(), "object": "model", "owned_by": "airllm"}],
            }))
        else:
            self._send(404, json.dumps({"error": "not found"}))

    def do_POST(self):
        if self.path.split("?", 1)[0] != "/v1/chat/completions":
            self._send(404, json.dumps({"error": "not found"}))
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            req = json.loads(self.rfile.read(length) or b"{}")
        except Exception as e:  # noqa: BLE001
            self._send(400, json.dumps({"error": f"bad request body: {e}"}))
            return
        if not STATE["loaded"]:
            self._send(503, json.dumps({"error": "model still loading or failed"}))
            return

        messages = req.get("messages") or []
        stream = bool(req.get("stream"))
        sample = {}
        if req.get("temperature") is not None:
            sample["temperature"] = float(req["temperature"])
            sample["do_sample"] = True
        if req.get("top_p") is not None:
            sample["top_p"] = float(req["top_p"])
            sample["do_sample"] = True
        if req.get("top_k") is not None:
            sample["top_k"] = int(req["top_k"])
        if not sample:
            sample["do_sample"] = False

        chunk_id = f"chatcmpl-{uuid.uuid4().hex[:24]}"
        try:
            input_ids = chat_tokens(messages)
        except Exception as e:  # noqa: BLE001
            self._send(400, json.dumps({"error": f"failed to build prompt: {e}"}))
            return

        if stream:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            # No Content-Length and no chunked encoding: the body is
            # close-delimited, which the supervisor's proxy passes through.
            self.send_header("Access-Control-Allow-Origin", "*")
            self.end_headers()
            try:
                def emit(piece):
                    self.wfile.write(sse_chunk(chunk_id, alias(),
                                               {"content": piece}).encode())
                    self.wfile.flush()

                self.wfile.write(sse_chunk(chunk_id, alias(),
                                           {"role": "assistant"}).encode())
                self.wfile.flush()
                generate_tokens(input_ids, sample, emit)
                self.wfile.write(sse_chunk(chunk_id, alias(), finish="stop").encode())
                self.wfile.write(b"data: [DONE]\n\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            except Exception as e:  # noqa: BLE001
                # The stream is already committed; close it so the client
                # sees a truncated answer rather than a hang.
                print(f"[airllm] generation failed: {e}", flush=True)
            finally:
                self.close_connection = True
        else:
            try:
                pieces = []
                generate_tokens(input_ids, sample, pieces.append)
                text = "".join(pieces)
                obj = {
                    "id": chunk_id,
                    "object": "chat.completion",
                    "created": int(time.time()),
                    "model": alias(),
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": text},
                        "finish_reason": "stop",
                    }],
                }
                self._send(200, json.dumps(obj))
            except Exception as e:  # noqa: BLE001
                self._send(500, json.dumps({"error": f"generation failed: {e}"}))


def main():
    global ARGS
    p = argparse.ArgumentParser(description="AirLLM OpenAI-compatible sidecar")
    p.add_argument("--model", required=True, help="HF model folder or repo id")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8788)
    p.add_argument("--compression", choices=["4bit", "8bit"], default=None)
    p.add_argument("--max-seq-len", type=int, default=4096)
    p.add_argument("--max-new-tokens", type=int, default=2048)
    p.add_argument("--shards-dir", default=None,
                   help="where split layer shards are written (default: next to the checkpoint)")
    ARGS = p.parse_args()

    server = ThreadingHTTPServer((ARGS.host, ARGS.port), Handler)
    threading.Thread(target=load_model, daemon=True).start()
    print(f"[airllm] sidecar listening on http://{ARGS.host}:{ARGS.port} "
          f"(model loads in the background)", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
