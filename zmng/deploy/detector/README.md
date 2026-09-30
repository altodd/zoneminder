# Reference object-detection server

`server.py` serves the DeepStack / CodeProject.AI detection API on top of ONNX Runtime, so zmng's `[objects]` section can point at it. Any other server with the same API (CodeProject.AI, DeepStack) works too.

```
python3 -m venv /opt/zmng-detector && . /opt/zmng-detector/bin/activate
pip install onnxruntime-gpu numpy pillow            # CUDA 12 build; use `onnxruntime` for CPU
pip install ultralytics && yolo export model=yolo11s.pt format=onnx imgsz=640 opset=17   # once, anywhere (a container is fine); copy the .onnx here
python3 server.py --model yolo11s.onnx --port 32168
curl -F image=@frame.jpg http://127.0.0.1:32168/v1/vision/detection
```
On the church server the Quadro P2200 (Pascal) is supported by the CUDA execution provider of onnxruntime-gpu 1.17–1.20 with CUDA 12 (TensorRT 10 dropped Pascal; not needed here). YOLOv8n at 640 px runs in ~25 ms on it, YOLOv8s ~60 ms; with zmng's default `interval_secs = 2` and 23 cameras that is well under 10 % of the GPU. Run it as a systemd service (`deploy/detector/zmng-detector.service`).

**CPU only is fine for a handful of cameras.** Detection is motion-gated (at most one frame per `interval_secs` per camera while motion lasts) plus one pass per finished event on its 640 px thumbnail. Measured on the church server (2× Xeon Silver 4116, loaded by ZoneMinder, nice 19, 2 threads): YOLO11n ~150 ms, YOLO11s ~420 ms per frame; both found both people in the Front Door test event at ~0.9. The test deployment runs YOLO11s on the CPU so ZoneMinder's saturated GPU is untouched. Matching `zmng.toml`:

```toml
[objects]
url = "http://127.0.0.1:32168/v1/vision/detection"
min_confidence = 0.5
interval_secs = 2.0
timeout_ms = 3000
max_concurrent = 2
```

Events recorded before the detector existed can be labelled afterwards: Admin → Object detection → "Label the last 7 days of events" (`POST /api/events/classify {"days": 7}`).

`test_server.py` exercises the server with a synthetic ONNX graph shaped like a YOLOv8 export (no real model needed): letterboxing, decoding, NMS and the error paths. Run it after `pip install onnx onnxruntime numpy pillow`; it is not part of `cargo test` because it needs those Python packages.
