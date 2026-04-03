import io
import tempfile

from gradio_client import Client, handle_file
from PIL import Image

from fastapi import FastAPI, File, UploadFile
from fastapi.responses import JSONResponse

# -------------------------------------------------------------- 

with open("./huggingface-api-key.txt", "r") as f:
    api_token = f.read().strip()
client = Client("fey-aino/GOT-OCR", hf_token = api_token)

app = FastAPI()

# --------------------------------------------------------------

@app.post("/ocr")
async def ocr(file: UploadFile = File(...), image_size: tuple[int, int] = (750, 750)):
    try:
        raw = await file.read()
        img = Image.open(io.BytesIO(raw))
        img = img.resize(image_size, resample=Image.BICUBIC)
        
        with tempfile.NamedTemporaryFile(suffix=".jpeg", delete=False) as temp_file:
            
            img.save(temp_file.name)
            img_path = temp_file.name
            
            result = client.predict(
                image=handle_file(img_path),
                task="Plain Text OCR",
                ocr_type="ocr",
                ocr_box="Hello!!",
                ocr_color="red",
                api_name="/ocr_demo"
            )
            print(result)
            
    except Exception as e:
        return JSONResponse(status_code=500, content={"error": str(e)})