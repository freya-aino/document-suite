import operator
import mlflow

from pydantic import BaseModel, Field
from typing import List, Annotated, Union
from mlflow import genai
from langgraph.graph import StateGraph, START, END
from langchain_openai import ChatOpenAI
from langchain.messages import HumanMessage, AIMessage

from ..abstract import Agent, Language



class CA_Conversation_ResponseFormat(BaseModel):
    antwort: str = Field(..., description="Deine Antwort die die Konversation weiterführt und mehr informationen sammelt und Unklarheiten aufkößt.")
    informationen_key_points: List[str] = Field(..., description="Die Informationen oder Key-points welche du von der bisherigen Konversation erhalten kontest.")
    anzahl_relevanter_nachrichten: int = Field(..., description="Die N (0-4) letzten Nachrichten die relevant für das momentante Thema sind.", ge=0, le=4) # TODO - restrict dynamically or something

class CA_Classification_ResponseFormat(BaseModel):
    ist_frage: bool = Field(..., description="Ist eine Frage gestellt worden")
    frage_präzision: int = Field(..., description="Die präzision einer gestellten frage (0-9)", ge=0, le=9)
    fachbereich: str = Field(..., description="Der Fachbereich der Frage.")


class CA_State(BaseModel):
    Conversation: Annotated[List[Union[HumanMessage, AIMessage]], operator.add]
    InformationKeyPoints: Annotated[List[str], operator.add]
    Classification: Annotated[List[CA_Classification_ResponseFormat], operator.add]
    NumContextRelevantMessages: int

class ConversationAgent(Agent):

    def __init__(self, llm: ChatOpenAI):
        # TODO - add prompt version
        # TODO - add production prompt # prompt = mlflow.genai.load_prompt("prompts:/my_prompt@production")

        self.llm = llm
        self.conversationPrompt = genai.load_prompt(CA_Conversation_ResponseFormat.__name__)
        self.classificationPrompt = genai.load_prompt(CA_Classification_ResponseFormat.__name__)
        
        # genai.create_dataset()
    
        graph = StateGraph(CA_State)
        graph.add_node(self.ConversationNode.__name__, self.ConversationNode)
        graph.add_node(self.ClassificationNode.__name__, self.ClassificationNode)
        graph.add_edge(START, self.ConversationNode.__name__)
        graph.add_edge(START, self.ClassificationNode.__name__)
        graph.add_edge(self.ConversationNode.__name__, END)
        graph.add_edge(self.ClassificationNode.__name__, END)
        self.graph = graph.compile()

        self.state = CA_State(
            Conversation=[],
            InformationKeyPoints=[],
            Classification=[],
            NumContextRelevantMessages=1
        )
        
        super().__init__(
            llm=llm,
            responseFormats=[
                CA_Conversation_ResponseFormat,
                CA_Classification_ResponseFormat
            ]
        )

    def __call__(self):
        self.state = CA_State(**self.graph.invoke(self.state))
        return self.state
    
    @mlflow.trace(name="CA_ConversationNode", span_type="func")
    def ConversationNode(self, state: CA_State):

        # # Get the current span (created by the @mlflow.trace decorator)
        # span = mlflow.get_current_active_span()
        # # Set the attribute to the span
        # span.set_attributes({"model": model_id})
        
        # mlflow.update_current_trace(tags={"fruit": "apple"})


        # process user input
        res = self.llm.with_structured_output(self.conversationPrompt.response_format).invoke([
            {
                "role": "system",
                "content": self.conversationPrompt.format(
                    allow_partial=False,
                    questionairGoals=[], # TODO - read from pre-formatted txt file or smth.
                    informationKeyPoints=state.InformationKeyPoints
                )
            },
            *state.Conversation
        ])
        res = CA_Conversation_ResponseFormat(**res)

        return {
            "Conversation": [AIMessage(content=res.antwort)],
            "InformationKeyPoints": res.informationen_key_points,
            "NumContextRelevantMessages": res.anzahl_relevanter_nachrichten,
        }

    @mlflow.trace(name="CA_ClassificationNode", span_type="func")
    def ClassificationNode(self, state: CA_State):

        # classify current question
        res = self.llm.with_structured_output(self.classificationPrompt.response_format).invoke([
            {
                "role": "system",
                "content": self.classificationPrompt.format(
                    allow_partial=False,
                    lastUserMessage=state.Conversation[-1].content
                )
            }
        ])
        res = CA_Classification_ResponseFormat(**res)

        return {
            "Classification": [res]
        }
