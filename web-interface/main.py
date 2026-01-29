import dotenv
import streamlit as st
import time
import mcp
import asyncio

from typing import AsyncGenerator

from mcp.client.streamable_http import streamable_http_client
from mcp import ClientSession

from langchain_mcp_adapters.tools import load_mcp_tools
from langchain.agents import create_agent
from langchain_anthropic import ChatAnthropic
from langchain.messages import HumanMessage, AIMessage, AIMessageChunk



VECTOR_ENDPOINT = "https://example.com/vectorize"   # <-- change
UPLOAD_DISABLED = False
# HEADERS = {"Authorization": "Bearer YOUR_TOKEN"}


server_params = {
    "url": "https://localhost:8080",
    # "headers": {
    #     "X-Api-Key":"lsv2_pt_your_api_key"
    # }
}


def vectorize():

    UPLOAD_DISABLED=True

    time.sleep(1)

    UPLOAD_DISABLED=False


def sidebar():
    sb = st.sidebar
    sb.title("Upload Documents")
    sb.file_uploader("Upload Documents", accept_multiple_files=True, on_change=vectorize, key="file_uploader", disabled=UPLOAD_DISABLED)


def query_vector_storage():
    
    with st.status("Vector Storage"):
        st.write("Querying...")
        time.sleep(2)


def main():

    if "conversation" not in st.session_state:
        st.session_state.conversation = []

    sidebar()


    prompt = st.chat_input()
    if prompt:
        query_vector_storage()
        st.session_state.conversation.append({
            "role": "human",
            "content": prompt
        })

        reply = f"echo {prompt}"
        st.session_state.conversation.append({
            "role": "ai",
            "content": reply
        })

    # re draw chat
    for message in st.session_state.conversation:
        st.chat_message(message["role"]).write(message["content"])




async def run_mcp_client() -> AsyncGenerator[AIMessageChunk, None]:
    async with streamable_http_client(url="http://localhost:8080") as (read_stream, write_stream, _):
        async with ClientSession(read_stream, write_stream) as session:
            
            await session.initialize()

            tools = await load_mcp_tools(session)
            print(f"available tools: ", tools)

            model = ChatAnthropic(
                model_name="claude-haiku-4-5-20251001",
                timeout=20.0,
                stop=["<STOP>"],
                temperature=0.35
            )

            agent = create_agent(
                model,
                tools
            )

            messages = [
                HumanMessage(content=["name what model you are and what tools where provided to you!"])
            ]
            
            async for chunk in model.astream(messages):
                yield chunk
            

def start_streaming():
    loop = asyncio.get_event_loop()

    async def _run():
        async for chunk in run_mcp_client():
            print("chunk:", chunk.content)

    loop.run_until_complete(_run())


if __name__ == "__main__":

    dotenv.load_dotenv()

    start_streaming()

    time.sleep(5)

    # main()
