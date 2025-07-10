FROM pytorch/pytorch:2.4.1-cuda12.4-cudnn9-devel

RUN apt-get update

RUN pip install \
    pillow \
    timm \
    fastapi \
    uvicorn \
    nltk \
    python-multipart

WORKDIR /app

RUN python -c "import timm; model = timm.create_model('tf_efficientnetv2_m.in21k', pretrained=True); import torch; torch.save(model.state_dict(), '/app/tf_efficientnetv2_m_in21k.pth')"
RUN python -c "import nltk; nltk.download('wordnet')"

COPY . .

# ENTRYPOINT [ "/bin/bash" ]
ENTRYPOINT [ "uvicorn", "run:app", "--host", "0.0.0.0", "--port", "8080" ]