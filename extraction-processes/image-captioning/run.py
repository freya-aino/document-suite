import io
import requests
import torch as T

from PIL import Image
from transformers import Blip2Processor, Blip2ForConditionalGeneration
from fastapi import FastAPI, File, UploadFile
from fastapi.responses import JSONResponse

# --------------------------------------------------------

app = FastAPI()
device = "cuda" if T.cuda.is_available() else "cpu"

# --------------------------------------------------------

processor = Blip2Processor.from_pretrained("Salesforce/blip2-opt-2.7b")
model = Blip2ForConditionalGeneration.from_pretrained("Salesforce/blip2-opt-2.7b", device_map=device)

# --------------------------------------------------------

@app.post("/image-captioning/")
async def image_captioning(file: UploadFile = File(...), max_now_tokens: int = 50, question: str = ""):
    
    try:
        
        raw = await file.read()
        raw_image = Image.open(io.BytesIO(raw)).convert('RGB')
        
        inputs = processor(raw_image, question, return_tensors="pt").to(device)
        out = model.generate(**inputs)
        out = processor.decode(out[0], skip_special_tokens=True).strip()
        
        return JSONResponse(content={"result": out}, status_code=200)
        
    except Exception as e:
        return JSONResponse(content={"error": str(e)}, status_code=500)
