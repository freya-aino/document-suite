FROM pytorch/pytorch:2.4.1-cuda12.4-cudnn9-devel


RUN apt-get update
RUN apt-get install -y \
    libgl1-mesa-glx \
    libglib2.0-0

WORKDIR /app

RUN pip install ultralytics
RUN python -c "from ultralytics import YOLO; YOLO('yolov8x.pt')"
RUN python -c "from ultralytics import YOLO; YOLO('yolov8x-seg.pt')"
RUN python -c "from ultralytics import YOLO; YOLO('yolov8x-pose-p6.pt')"
RUN python -c "from ultralytics import YOLO; YOLO('yolov8x-obb.pt')"
RUN python -c "from ultralytics import YOLO; YOLO('yolov8x-cls.pt')"


# install fastapi
RUN pip install \
    fastapi \
    uvicorn \
    python-multipart


COPY . .
CMD [ "uvicorn", "routs:app", "--host", "0.0.0.0", "--port", "8080" ]