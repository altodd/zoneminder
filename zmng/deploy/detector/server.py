#!/usr/bin/env python3
"""Reference object-detection server for zmng (DeepStack / CodeProject.AI API).

    POST /v1/vision/detection   multipart field `image` (JPEG/PNG), optional `min_confidence`
    -> {"success": true, "predictions": [{"label", "confidence", "x_min", "y_min", "x_max", "y_max"}], "duration": ms}
    GET  /health                -> {"ok": true, "model": ..., "providers": [...]}

Runs a YOLO ONNX export (ultralytics: `yolo export model=yolov8n.pt format=onnx`,
also YOLO11 and the YOLOv10 end-to-end export) on ONNX Runtime, using the CUDA
execution provider when the `onnxruntime-gpu` wheel is installed and a GPU is
present, else the CPU. Model input is letterboxed to the export size (640 by
default), output is decoded for both layouts:

    [1, 4 + classes, N]   YOLOv8/YOLO11 (needs NMS, done here)
    [1, N, 6]             YOLOv10 / NMS-fused exports (x0 y0 x1 y1 conf class)

Usage:
    pip install onnxruntime-gpu numpy pillow      # or onnxruntime for CPU
    python3 server.py --model yolov8n.onnx --port 32168 [--labels coco.txt] [--threads 2]

Point zmng at http://<host>:32168/v1/vision/detection.
"""
import argparse
import io
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
from PIL import Image

COCO = ("person bicycle car motorcycle airplane bus train truck boat traffic-light fire-hydrant stop-sign "
        "parking-meter bench bird cat dog horse sheep cow elephant bear zebra giraffe backpack umbrella handbag "
        "tie suitcase frisbee skis snowboard sports-ball kite baseball-bat baseball-glove skateboard surfboard "
        "tennis-racket bottle wine-glass cup fork knife spoon bowl banana apple sandwich orange broccoli carrot "
        "hot-dog pizza donut cake chair couch potted-plant bed dining-table toilet tv laptop mouse remote keyboard "
        "cell-phone microwave oven toaster sink refrigerator book clock vase scissors teddy-bear hair-drier "
        "toothbrush").split()


def letterbox(img, size):
    """Resize keeping aspect, pad to size x size; returns (chw float array, scale, pad_x, pad_y)."""
    w, h = img.size
    scale = min(size / w, size / h)
    nw, nh = max(1, round(w * scale)), max(1, round(h * scale))
    canvas = Image.new("RGB", (size, size), (114, 114, 114))
    px, py = (size - nw) // 2, (size - nh) // 2
    canvas.paste(img.resize((nw, nh), Image.BILINEAR), (px, py))
    arr = np.asarray(canvas, dtype=np.float32) / 255.0
    return arr.transpose(2, 0, 1)[None], scale, px, py


def nms(boxes, scores, iou_thr=0.45):
    """Greedy NMS on xyxy boxes; returns kept indices."""
    order = scores.argsort()[::-1]
    keep = []
    while order.size:
        i = order[0]
        keep.append(i)
        if order.size == 1:
            break
        rest = order[1:]
        xx1 = np.maximum(boxes[i, 0], boxes[rest, 0])
        yy1 = np.maximum(boxes[i, 1], boxes[rest, 1])
        xx2 = np.minimum(boxes[i, 2], boxes[rest, 2])
        yy2 = np.minimum(boxes[i, 3], boxes[rest, 3])
        inter = np.clip(xx2 - xx1, 0, None) * np.clip(yy2 - yy1, 0, None)
        a = (boxes[i, 2] - boxes[i, 0]) * (boxes[i, 3] - boxes[i, 1])
        b = (boxes[rest, 2] - boxes[rest, 0]) * (boxes[rest, 3] - boxes[rest, 1])
        iou = inter / np.maximum(a + b - inter, 1e-9)
        order = rest[iou < iou_thr]
    return keep


def decode(output, min_conf):
    """Turn a model output into (xyxy boxes in letterbox pixels, scores, class ids)."""
    out = np.asarray(output)
    if out.ndim == 3 and out.shape[2] == 6:            # [1, N, 6] end-to-end export
        det = out[0]
        det = det[det[:, 4] >= min_conf]
        return det[:, :4], det[:, 4], det[:, 5].astype(int), False
    if out.ndim == 3 and out.shape[1] < out.shape[2]:  # [1, 4+C, N]
        out = out[0].T                                  # -> [N, 4+C]
    else:
        out = out[0]
    xywh, cls = out[:, :4], out[:, 4:]
    ids = cls.argmax(1)
    scores = cls[np.arange(len(ids)), ids]
    m = scores >= min_conf
    xywh, scores, ids = xywh[m], scores[m], ids[m]
    boxes = np.stack([xywh[:, 0] - xywh[:, 2] / 2, xywh[:, 1] - xywh[:, 3] / 2,
                      xywh[:, 0] + xywh[:, 2] / 2, xywh[:, 1] + xywh[:, 3] / 2], 1) if len(xywh) else np.zeros((0, 4))
    return boxes, scores, ids, True


class Detector:
    def __init__(self, model, labels, threads):
        import onnxruntime as ort
        opts = ort.SessionOptions()
        opts.intra_op_num_threads = threads
        providers = [p for p in ("CUDAExecutionProvider", "CPUExecutionProvider") if p in ort.get_available_providers()]
        self.sess = ort.InferenceSession(model, opts, providers=providers)
        self.providers = self.sess.get_providers()
        inp = self.sess.get_inputs()[0]
        self.input_name = inp.name
        self.size = int(inp.shape[2]) if isinstance(inp.shape[2], int) else 640
        self.labels = labels
        self.lock = threading.Lock()

    def detect(self, img, min_conf):
        x, scale, px, py = letterbox(img.convert("RGB"), self.size)
        with self.lock:
            out = self.sess.run(None, {self.input_name: x})[0]
        boxes, scores, ids, need_nms = decode(out, min_conf)
        if need_nms and len(boxes):
            keep = nms(boxes, scores)
            boxes, scores, ids = boxes[keep], scores[keep], ids[keep]
        w, h = img.size
        preds = []
        for b, s, c in zip(boxes, scores, ids):
            x0 = max(0, (b[0] - px) / scale)
            y0 = max(0, (b[1] - py) / scale)
            x1 = min(w, (b[2] - px) / scale)
            y1 = min(h, (b[3] - py) / scale)
            if x1 <= x0 or y1 <= y0:
                continue
            label = self.labels[int(c)] if int(c) < len(self.labels) else str(int(c))
            preds.append({"label": label, "confidence": round(float(s), 4),
                          "x_min": int(x0), "y_min": int(y0), "x_max": int(x1), "y_max": int(y1)})
        return preds


def parse_multipart(body, content_type):
    """Minimal multipart/form-data parser: returns {name: bytes}."""
    boundary = None
    for part in content_type.split(";"):
        part = part.strip()
        if part.startswith("boundary="):
            boundary = part[len("boundary="):].strip('"')
    if not boundary:
        return {}
    fields = {}
    for chunk in body.split(b"--" + boundary.encode()):
        chunk = chunk.strip(b"\r\n")
        if not chunk or chunk == b"--":
            continue
        head, _, data = chunk.partition(b"\r\n\r\n")
        name = None
        for line in head.split(b"\r\n"):
            if line.lower().startswith(b"content-disposition"):
                for kv in line.split(b";"):
                    kv = kv.strip()
                    if kv.startswith(b"name="):
                        name = kv[5:].strip(b'"').decode()
        if name:
            fields[name] = data
    return fields


class Handler(BaseHTTPRequestHandler):
    detector = None

    def _json(self, code, obj):
        data = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.startswith("/health"):
            self._json(200, {"ok": True, "providers": self.detector.providers, "input": self.detector.size})
        else:
            self._json(404, {"success": False, "error": "not found"})

    def do_POST(self):
        if not self.path.startswith("/v1/vision/detection"):
            return self._json(404, {"success": False, "error": "not found"})
        t0 = time.time()
        n = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(n)
        fields = parse_multipart(body, self.headers.get("Content-Type", ""))
        if "image" not in fields:
            return self._json(400, {"success": False, "error": "no image field"})
        try:
            min_conf = float(fields.get("min_confidence", b"0.4"))
        except ValueError:
            min_conf = 0.4
        try:
            img = Image.open(io.BytesIO(fields["image"]))
            preds = self.detector.detect(img, min_conf)
        except Exception as e:  # noqa: BLE001 - report to the client, keep serving
            return self._json(500, {"success": False, "error": str(e)})
        self._json(200, {"success": True, "predictions": preds, "duration": int((time.time() - t0) * 1000)})

    def log_message(self, fmt, *args):  # quieter than the default
        if "--verbose" in sys.argv:
            super().log_message(fmt, *args)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--model", required=True, help="YOLO ONNX file")
    ap.add_argument("--labels", help="text file, one label per line (default: COCO 80)")
    ap.add_argument("--port", type=int, default=32168)
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--threads", type=int, default=2)
    ap.add_argument("--verbose", action="store_true")
    a = ap.parse_args()
    labels = [l.strip() for l in open(a.labels) if l.strip()] if a.labels else COCO
    Handler.detector = Detector(a.model, labels, a.threads)
    print(f"model {a.model} input {Handler.detector.size} providers {Handler.detector.providers}; listening on {a.host}:{a.port}", flush=True)
    ThreadingHTTPServer((a.host, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
