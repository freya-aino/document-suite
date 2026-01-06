import re
import io

from fastapi import FastAPI, File, UploadFile
from fastapi.responses import JSONResponse
from PIL import Image

from transformers import DonutProcessor, VisionEncoderDecoderModel
import torch as T


# ----------------------------------

app = FastAPI()

device = "cuda" if T.cuda.is_available() else "cpu"

# ----------------------------------

processor_parse = DonutProcessor.from_pretrained("naver-clova-ix/donut-base-finetuned-cord-v2")
model_parse = VisionEncoderDecoderModel.from_pretrained("naver-clova-ix/donut-base-finetuned-cord-v2").to(device)


processor_vqa = DonutProcessor.from_pretrained("naver-clova-ix/donut-base-finetuned-docvqa")
model_vqa = VisionEncoderDecoderModel.from_pretrained("naver-clova-ix/donut-base-finetuned-docvqa").to(device)



@app.post("/parse-document/")
async def parse_document(file: UploadFile = File(...)):
    try:
        image = await file.read()
        image = Image.open(io.BytesIO(image))
        
        pixel_values = processor_parse(image, return_tensors="pt").pixel_values
        
        # prepare decoder inputs
        task_prompt = "<s_cord-v2>"
        decoder_input_ids = processor_parse.tokenizer(task_prompt, add_special_tokens=False, return_tensors="pt").input_ids
        
        outputs = model_parse.generate(
            pixel_values.to(device),
            decoder_input_ids=decoder_input_ids.to(device),
            max_length=model_parse.decoder.config.max_position_embeddings,
            pad_token_id=processor_parse.tokenizer.pad_token_id,
            eos_token_id=processor_parse.tokenizer.eos_token_id,
            use_cache=True,
            bad_words_ids=[[processor_parse.tokenizer.unk_token_id]],
            return_dict_in_generate=True,
        )
        
        sequence = processor_parse.batch_decode(outputs.sequences)[0]
        sequence = sequence.replace(processor_parse.tokenizer.eos_token, "").replace(processor_parse.tokenizer.pad_token, "")
        sequence = re.sub(r"<.*?>", "", sequence, count=1).strip()  # remove first task start token
        
        return JSONResponse(content={"tokens": processor_parse.token2json(sequence)}, status_code=200)
        
    except Exception as e:
        return JSONResponse(content={"error": str(e)}, status_code=500)
    
@app.post("/question-document/")
async def question_document(file: UploadFile = File(...), question: str = ""):
    
    try:
        
        image = await file.read()
        image = Image.open(io.BytesIO(image))
        
        pixel_values = processor_vqa(image, return_tensors="pt").pixel_values
        
        # prepare decoder inputs
        prompt = f"<s_docvqa><s_question>{question}</s_question><s_answer>"
        decoder_input_ids = processor_vqa.tokenizer(prompt, add_special_tokens=False, return_tensors="pt").input_ids
        
        outputs = model_vqa.generate(
            pixel_values.to(device),
            decoder_input_ids=decoder_input_ids.to(device),
            max_length=model_vqa.decoder.config.max_position_embeddings,
            pad_token_id=processor_vqa.tokenizer.pad_token_id,
            eos_token_id=processor_vqa.tokenizer.eos_token_id,
            use_cache=True,
            bad_words_ids=[[processor_vqa.tokenizer.unk_token_id]],
            return_dict_in_generate=True,
        )
        
        sequence = processor_vqa.batch_decode(outputs.sequences, clean_up_tokenization_spaces=True)[0]
        sequence = sequence.replace(processor_vqa.tokenizer.eos_token, "").replace(processor_vqa.tokenizer.pad_token, "")
        sequence = re.sub(r"<.*?>", "", sequence, count=1).strip()  # remove first task start token
        
        return JSONResponse(content={"tokens": processor_vqa.token2json(sequence)}, status_code=200)
    
    except Exception as e:
        return JSONResponse(content={"error": str(e)}, status_code=500)