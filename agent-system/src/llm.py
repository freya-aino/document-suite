import os
from langchain_openai import ChatOpenAI

def OpenAI_LLM(
        base_url: str,
        model: str = "gpt-4.1",
        temperature: float = 0.65,
        top_p: float = 0.95,
        max_completion_tokens: int = 1000,
        timeout_s: int = 20,
        max_retries: int = 1,
        seed: int = 0
    ) -> ChatOpenAI:
    return ChatOpenAI(
        base_url=base_url,
        model=model,
        temperature=temperature,
        max_completion_tokens=max_completion_tokens,
        timeout=timeout_s,
        max_retries=max_retries,
        seed=seed,
        top_p=top_p,
        # frequency_penalty=
        # streaming=True 
    )

# print(agent.get_graph().draw_mermaid())
# exit()

