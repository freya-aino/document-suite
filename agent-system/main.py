import dotenv
import os
import mlflow
import mlflow.openai as mlfopenai
import mlflow.langchain as mlflangchain

from langchain.messages import HumanMessage

from src import llm
from src import agents

if __name__ == "__main__":

    # prompts = util.loadInitialPrompts("./prompts")

    # mlflow.genai.set_prompt_model_config(
    #     name="my-prompt",
    #     version=1,
    #     model_config={"model_name": "gpt-4", "temperature": 0.7},
    # )


    # VECTOR_ENDPOINT = "https://example.com/vectorize"   # <-- change
    # UPLOAD_DISABLED = False
    # # HEADERS = {"Authorization": "Bearer YOUR_TOKEN"}


    # TODO
    # @scorer
    # def eval_question_extraction(user_question, generated_question):

    
    # load environment variables
    dotenv.load_dotenv()

    # initialize mlflow
    mlflow.set_tracking_uri(f"http://{os.environ['MLFLOW_HOST']}:{os.environ['MLFLOW_PORT']}") 

    mlflow.set_experiment("test experiment") # TODO - dont hardcode experiment
    # mlfopenai.autolog()
    # mlflangchain.autolog()
    print("mlflow - initialized")

    # from src.prompts import load_all_prompts
    # load_all_prompts()
    # print("mlflow - prompts loaded")

    LLM = llm.OpenAI_LLM(os.environ["OPENAI_ENDPOINT"])
    # mlflangchain.log_model(LLM)

    # LLM.invoke([{"role": "user", "content": "give me a list of ingreidence for a general curry dish"}])

    conversationAgent = agents.ConversationAgent(LLM, 1)
    
    out = conversationAgent("hi how are you")
    
    print(out)
    # print(conversationAgent.state)

