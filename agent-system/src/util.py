# import mcp
# import asyncio
# import pandas as pd

# from typing import AsyncGenerator

# from mcp.client.streamable_http import streamable_http_client
# from mcp import ClientSession

# from langchain_mcp_adapters.tools import load_mcp_tools
# from langchain.agents import create_agent
# from langchain_anthropic import ChatAnthropic
# from langchain.messages import HumanMessage, AIMessage, AIMessageChunk

# from typing import List


from mlflow import genai
from pydantic import BaseModel
import os


# def evaluate_examples(examples: pd.DataFrame):
#     beispiele = [
#         {
#             "frage": r['orginal_frage'],
#             "gedanken": r['gedanken'],
#             "antwort": r['antwort'],
#             "bewertung": {
#                 "bezug_auf_quellen": str(r['bewertung_bezug_auf_quellen']),
#                 "bezug_auf_sachverhalt": str(r['bewertung_bezug_auf_sachverhalt']),
#                 "gedankengang_effizienz": str(r['bewertung_gedankengang_effizienz']),
#             }
#         }
#         for (_, r) in examples.iterrows()
#     ]

#     outputs = []

#     # TODO
#     # agent = WissensAgent(
#     #     max_llm_calls = 3,
#     #     erwuenschte_note = 2.4
#     # ).compile()


# async def run_mcp_client(http_client_url: str) -> AsyncGenerator[AIMessageChunk, None]:
#     async with streamable_http_client(url=http_client_url) as (read_stream, write_stream, _):
#         async with ClientSession(read_stream, write_stream) as session:
            
#             await session.initialize()

#             tools = await load_mcp_tools(session)
#             print(f"available tools: ", tools)

#             model = ChatAnthropic(
#                 model_name="claude-haiku-4-5-20251001",
#                 timeout=20.0,
#                 stop=["<STOP>"],
#                 temperature=0.35
#             )

#             agent = create_agent(
#                 model,
#                 tools
#             )

#             messages = [
#                 HumanMessage(content=["name what model you are and what tools where provided to you!"])
#             ]
            
#             async for chunk in model.astream(messages):
#                 yield chunk
            

# TODO
# def print_stream():
#     loop = asyncio.get_event_loop()
#     async def _run():
#         async for chunk in run_mcp_client(http_client_url="http://localhost:8080"):
#             print("chunk:", chunk.content)
#             # yield chunk
#     loop.run_until_complete(_run())
