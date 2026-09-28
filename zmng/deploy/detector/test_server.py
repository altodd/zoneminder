#!/usr/bin/env python3
"""Self-test for server.py without a real model.

Builds a synthetic ONNX graph shaped like a YOLOv8 export ([1,84,8400]) that
always reports one confident person (plus a duplicate for NMS and a
low-confidence car), starts server.py on a free port, and checks the
DeepStack-style API end to end (letterbox, decode, NMS, un-letterbox, error
handling).  Needs: pip install onnx onnxruntime numpy pillow

    python3 test_server.py
"""
import io
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

import numpy as np
import onnx
from onnx import TensorProto, helper
from PIL import Image


def fake_model(path):
    out = np.zeros((1, 84, 8400), dtype=np.float32)
    out[0, :4, 0], out[0, 4, 0] = (320, 400, 100, 200), 0.9   # person, centre of the letterbox
    out[0, :4, 1], out[0, 4, 1] = (322, 402, 100, 200), 0.8   # duplicate -> NMS
    out[0, :4, 2], out[0, 6, 2] = (100, 100, 40, 40), 0.2     # car below threshold
    nodes = [
        helper.make_node("Constant", [], ["c"], value=helper.make_tensor("v", TensorProto.FLOAT, out.shape, out.flatten().tolist())),
        helper.make_node("ReduceMean", ["images"], ["m"], keepdims=0),
        helper.make_node("Constant", [], ["z"], value=helper.make_tensor("zv", TensorProto.FLOAT, [], [0.0])),
        helper.make_node("Mul", ["m", "z"], ["mz"]),
        helper.make_node("Add", ["c", "mz"], ["output0"]),
    ]
    g = helper.make_graph(nodes, "fake_yolo",
                          [helper.make_tensor_value_info("images", TensorProto.FLOAT, [1, 3, 640, 640])],
                          [helper.make_tensor_value_info("output0", TensorProto.FLOAT, [1, 84, 8400])])
    m = helper.make_model(g, opset_imports=[helper.make_opsetid("", 13)])
    m.ir_version = 8
    onnx.save(m, path)


def post_image(url, jpeg, extra=None):
    boundary = "zmngtest"
    body = b""
    for k, v in (extra or {}).items():
        body += f"--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n".encode()
    if jpeg is not None:
        body += f"--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"f.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n".encode() + jpeg + b"\r\n"
    body += f"--{boundary}--\r\n".encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": f"multipart/form-data; boundary={boundary}"})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read())


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    with tempfile.TemporaryDirectory() as d:
        model = os.path.join(d, "fake.onnx")
        fake_model(model)
        s = socket.socket(); s.bind(("127.0.0.1", 0)); port = s.getsockname()[1]; s.close()
        proc = subprocess.Popen([sys.executable, os.path.join(here, "server.py"), "--model", model, "--host", "127.0.0.1", "--port", str(port)])
        try:
            for _ in range(50):
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1).read()
                    break
                except Exception:
                    time.sleep(0.2)
            img = Image.new("RGB", (640, 360), (30, 60, 90))
            buf = io.BytesIO(); img.save(buf, "JPEG"); jpeg = buf.getvalue()
            code, res = post_image(f"http://127.0.0.1:{port}/v1/vision/detection", jpeg, {"min_confidence": "0.4"})
            assert code == 200 and res["success"], res
            preds = res["predictions"]
            assert len(preds) == 1, preds                     # NMS removed the duplicate, the car is below 0.4
            p = preds[0]
            assert p["label"] == "person" and p["confidence"] == 0.9, p
            # letterbox of 640x360 into 640x640 pads 140 px top/bottom: centre (320,400) -> (320,260)
            assert (p["x_min"], p["y_min"], p["x_max"], p["y_max"]) == (270, 160, 370, 360), p
            code, res = post_image(f"http://127.0.0.1:{port}/v1/vision/detection", None)
            assert code == 400 and not res["success"], res
            code, res = post_image(f"http://127.0.0.1:{port}/v1/vision/detection", b"not a jpeg")
            assert code == 500 and not res["success"], res
            print("server.py OK")
        finally:
            proc.terminate()
            proc.wait(timeout=5)


if __name__ == "__main__":
    main()
