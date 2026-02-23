import os

from pathlib import Path
from mlflow import genai
from abc import ABC
from typing import List
from pydantic import BaseModel
from enum import Enum

from langchain_openai import ChatOpenAI


class Language(Enum):
    DE = "de"

class Agent(ABC):
    def __init__(self, llm: ChatOpenAI, responseFormats: List[type[BaseModel]]):
        self.responseFormats = responseFormats
        self.llm = llm

    def __call__(self):
        raise NotImplementedError
    
    def InitializePrompts(self, language: str = "de"): # TODO - dont hardcode language

        assert len(self.responseFormats) > 0, "the list of respone formats for this agent is empty, make sure this is desired behaviour."

        for respFormat in self.responseFormats:

            # local_path = Path(__file__).parent / prompt.Language.value / "prompts" # TODO - add languages to local file structure
            local_path = Path(__file__).parent / "prompts"
            assert os.path.isdir(local_path)

            with open(local_path / respFormat.__name__) as f:
                template = f.read()

            genai.register_prompt(
                name=respFormat.__name__,
                template=template,
                response_format=respFormat,
                commit_message="Initial commit",
                tags={
                    "language": language,
                },
            )


def format_system_prompt(prompt, **args):
    return {
        "role": "system",
        "content": prompt.format(**args)
    }