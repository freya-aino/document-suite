import cv2
import numpy as np
import torch as T

from torch import tensor, float32
from torchvision.transforms import Resize
from fastapi import UploadFile, FastAPI, HTTPException, File
from fastapi.responses import JSONResponse
from typing import Union
from ultralytics import YOLO

# ----------------------------------------------------------------

device = "cuda" if T.cuda.is_available() else "cpu"

detection_model = None
pose_model = None
classification_model = None
segmentation_model = None
obb_model = None

app = FastAPI()

# ----------------------------------------------------------------

async def file_to_image(file: UploadFile, out_size=(640, 640)):
    
    # load image from buffer
    contents = await file.read()
    
    image = np.frombuffer(contents, np.uint8)
    image = cv2.imdecode(image, cv2.IMREAD_COLOR)
    image = cv2.cvtColor(image, cv2.COLOR_BGR2RGB)
    
    image = tensor(image, dtype=float32).to(device)
    image = image.permute(2, 0, 1).unsqueeze(0)
    image = Resize(out_size)(image) / 255.0
    
    image = T.clamp(image, 0.0, 1.0)
    
    return image

# ----------------------------------------------------------------

@app.on_event("startup")
def load_models():
    global detection_model
    global pose_model
    global classification_model
    global detectionOpenImagev7_model
    global segmentation_model
    global obb_model
    
    detection_model = YOLO("yolov8x.pt").to(device)
    obb_model = YOLO("yolov8x-obb.pt").to(device)
    pose_model = YOLO("yolov8x-pose-p6.pt").to(device)
    classification_model = YOLO("yolov8x-cls.pt").to(device)
    segmentation_model = YOLO("yolov8x-seg.pt").to(device)

# ----------------------------------------------------------------

@app.post("/detection")
async def detection(file: UploadFile = File(...)):
    
    try:
        assert detection_model is not None, "Model not loaded"
        image_tensor = await file_to_image(file)
        ret = detection_model.predict(image_tensor)
        
        results = []
        for i in range(len(ret)):
            
            if ret[i] is None or ret[i].boxes is None or len(ret[i].boxes) == 0:
                continue
            
            results.append(
                {
                    "bboxes": [{
                        "bbox": box.xywhn.cpu().numpy().tolist()[0],
                        "confidence": box.conf.cpu().numpy().tolist()[0],
                        "class": ret[i].names[box.cls.cpu().numpy().tolist()[0]]
                    } for box in ret[i].boxes]
                }
            )
        
        return JSONResponse(content={"result": results}, status_code=200)
    except Exception as e:
        return JSONResponse(content={"message": str(e)}, status_code=500)

@app.post("/pose")
async def pose(file: UploadFile = File(...)):
    
    try:
        assert pose_model is not None, "Model not loaded"
        image_tensor = await file_to_image(file)
        ret = pose_model.predict(image_tensor)
        
        results = []
        for i in range(len(ret)):
            
            if ret[i] is None or ret[i].boxes is None or ret[i].keypoints is None or len(ret[i].boxes) == 0 or len(ret[i].keypoints) == 0:
                continue
            
            results.append({
                "bboxes": [{
                        "bbox": box.xywhn.cpu().numpy().tolist()[0],
                        "confidence": box.conf.cpu().numpy().tolist()[0],
                    } for box in ret[i].boxes],
                "landmarks": [{
                        "keypoints": lan.xyn.cpu().numpy().tolist(),
                        "confidence": lan.conf.cpu().numpy().tolist()
                    } for lan in ret[i].keypoints]
            })
            
        return JSONResponse(content={"result": results}, status_code=200)
        
    except Exception as e:
        return JSONResponse(content={"message": str(e)}, status_code=500)

@app.post("/classification")
async def classification(file: UploadFile = File(...)):
    
    try:
        assert classification_model  is not None, "Model not loaded"
        image_tensor = await file_to_image(file)
        ret = classification_model.predict(image_tensor)
        
        results = []
        for i in range(len(ret)):
            
            if ret[i] is None or ret[i].probs is None:
                continue
            
            results.append({
                "Top5": list(map(lambda a: ret[i].names[a], ret[i].probs.top5)),
                "top5conf": ret[i].probs.top5conf.cpu().numpy().tolist()
            })
        
        return JSONResponse(content={"result": results}, status_code=200)
    except Exception as e:
        return JSONResponse(content={"message": str(e)}, status_code=500)

@app.post("/segmentation")
async def segmentation(file: UploadFile = File(...)):
    
    try:
        assert segmentation_model is not None, "Model not loaded"
        image_tensor = await file_to_image(file)
        ret = segmentation_model.predict(image_tensor)
        
        results = []
        for i in range(len(ret)):
            
            if ret[i] is None or ret[i].masks is None or ret[i].masks is None:
                continue
            
            results.append({
                "masks": [{
                    "class": ret[i].names[bbox.cls.cpu().numpy().tolist()[0]],
                    "confidence": bbox.conf.cpu().numpy().tolist()[0],
                    "bbox": bbox.xywhn.cpu().numpy().tolist()[0],
                    "mask": mask.xyn[0].tolist()
                } for (bbox, mask) in zip(ret[i].boxes, ret[i].masks)
            ]})
        
        return JSONResponse(content={"result": results}, status_code=200)
    except Exception as e:
        return JSONResponse(content={"message": str(e)}, status_code=500)

@app.post("/obb")
async def obb(file: UploadFile = File(...)):
    
    try:
        assert obb_model is not None, "Model not loaded"
        image_tensor = await file_to_image(file)
        ret = obb_model.predict(image_tensor)
        
        results = []
        for i in range(len(ret)):
            
            if ret[i] is None or ret[i].obb is None:
                continue
            
            bboxes = [obb.xywhr.cpu().numpy().tolist()[0] for obb in ret[i].obb]
            
            results.append({
                "bboxes": [{
                    "class": ret[i].names[obb.cls.cpu().numpy().tolist()[0]],
                    "confidence": obb.conf.cpu().numpy().tolist()[0],
                    "bbox": [
                        bboxes[j][0] / image_tensor.shape[2], 
                        bboxes[j][1] / image_tensor.shape[3], 
                        bboxes[j][2] / image_tensor.shape[2], 
                        bboxes[j][3] / image_tensor.shape[3], 
                        bboxes[j][4]
                    ]
                } for (j, obb) in enumerate(ret[i].obb)
            ]})
            
        return JSONResponse(content={"result": results}, status_code=200)
    except Exception as e:
        return JSONResponse(content={"message": str(e)}, status_code=500)