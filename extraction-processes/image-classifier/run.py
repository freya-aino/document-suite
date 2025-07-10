import io
import timm
import torch as T

from urllib.request import urlopen
from PIL import Image
from fastapi import FastAPI, File, UploadFile
from fastapi.responses import JSONResponse
from nltk.corpus import wordnet as wn

# ------------------- setup ------------------- #
app = FastAPI()

device = "cuda" if T.cuda.is_available() else "cpu"

with open("./imagenet21k_wordnet_ids.txt") as f:
    wordnet_ids = [int(line.strip()[1:]) for line in f.readlines()] 

# ------------------- setup ------------------- #
weights_path = "./tf_efficientnetv2_m_in21k.pth"

model = timm.create_model('tf_efficientnetv2_m.in21k', pretrained=False)
model.load_state_dict(T.load(weights_path, weights_only=True))
model = model.eval()
model = model.to(device)

# get model specific transforms (normalization, resize)
data_config = timm.data.resolve_model_data_config(model)
transforms = timm.data.create_transform(**data_config, is_training=False)


# # test 
# img = Image.open(urlopen(
#     'https://huggingface.co/datasets/huggingface/documentation-images/resolve/main/beignets-task-guide.png'
# ))

# output = model(transforms(img).unsqueeze(0))  # unsqueeze single image into batch of 1
# top5_probabilities, top5_class_indices = T.topk(output.softmax(dim=1) * 100, k=5)

# print({
#     "probabilities": top5_probabilities.cpu().detach().numpy()[0].tolist(),
#     "labels": [wn.synset_from_pos_and_offset("n", wordnet_ids[i]) for i in top5_class_indices.cpu().detach().numpy()[0]]
# })


@app.post("/predict")
async def predict(file: UploadFile = File(...)):
    
    try:
        raw = await file.read()
        img = Image.open(io.BytesIO(raw)).convert('RGB')
        
        output = model(transforms(img).unsqueeze(0).to(device))
        top5_probabilities, top5_class_indices = T.topk(output.softmax(dim=1) * 100, k=5)
        
        labels = []
        for idx in top5_class_indices.cpu().detach().numpy()[0]:
            synset = wn.synset_from_pos_and_offset("n", wordnet_ids[idx])
            lemmas = [lemma.name() for lemma in synset.lemmas()]
            labels.append({
                "synset": synset.name(),
                "lemmas": lemmas
            })
        
        return JSONResponse(content={
            "probabilities": top5_probabilities.cpu().detach().numpy()[0].tolist(),
            "labels": labels
        })
        
    except Exception as e:
        return JSONResponse(content={"error": str(e)}, status_code=500)