import dotenv
import streamlit as st
import time


def query_vector_storage():
    
    with st.status("Vector Storage"):
        st.write("Querying...")
        time.sleep(2)


def upload_page():
    st.title("Upload")

    st.file_uploader("Upload Documents", accept_multiple_files=True, key="file_uploader", disabled=UPLOAD_DISABLED)


def chat_page():

    if "conversation" not in st.session_state:
        st.session_state.conversation = []

    prompt = st.chat_input()
    if prompt:

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

    # query_vector_storage()


if __name__ == "__main__":

    # dotenv.load_dotenv()

    # start_streaming()

    # time.sleep(5)

    pg = st.navigation([
        upload_page,
        chat_page
    ])
    pg.run()
