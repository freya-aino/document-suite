FROM pytorch/pytorch:2.11.0-cuda12.8-cudnn9-runtime

RUN apt-get update -y
RUN apt-get install -y git-lfs
RUN curl --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/huggingface/xet-core/refs/heads/main/git_xet/install.sh | sh
RUN git clone https://huggingface.co/zai-org/GLM-4.6V-Flash /opt/model-dir

ENTRYPOINT [ "/bin/bash" ]