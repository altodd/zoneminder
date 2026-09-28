# Reference object-detection server

`server.py` serves the DeepStack / CodeProject.AI detection API on top of ONNX Runtime, so zmng's `[objects]` section can point at it. Any other server with the same API (CodeProject.AI, DeepStack) works too.

```
python3 -m venv /opt/zmng-detector && . /opt/zmng-detector/bin/activate
pip install onnxruntime-gpu numpy pillow            # CUDA 12 build; use `onnxruntime` for CPU
pip install ultralytics && yolo export model=yolov8n.pt format=onnx imgsz=640   # once, anywhere; copy the .onnx here
python3 server.py --model yolov8n.onnx --port 32168
curl -F image=@frame.jpg http://127.0.0.1:32168/v1/vision/detection
```
On the church server the Quadro P2200 (Pascal) is supported by the CUDA execution provider of onnxruntime-gpu 1.17–1.20 with CUDA 12 (TensorRT 10 dropped Pascal; not needed here). YOLOv8n at 640 px runs in ~25 ms on it, YOLOv8s ~60 ms; with zmng's default `interval_secs = 2` and 23 cameras that is well under 10 % of the GPU. Run it as a systemd service (`deploy/detector/zmng-detector.service`).

`test_server.py` exercises the server with a synthetic ONNX graph shaped like a YOLOv8 export (no real model needed): letterboxing, decoding, NMS and the error paths. Run it after `pip install onnx onnxruntime numpy pillow`; it is not part of `cargo test` because it needs those Python packages.
