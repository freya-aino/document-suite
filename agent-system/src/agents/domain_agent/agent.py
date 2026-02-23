import operator
import mlflow

from pydantic import BaseModel, Field
from typing import List, Annotated, Union, Optional
from mlflow import genai
from langgraph.graph import StateGraph, START, END
from langchain_openai import ChatOpenAI
from langchain.messages import HumanMessage, AIMessage

from ..abstract import Agent, format_system_prompt



class DA_DomainClassification_ResponseFormat(BaseModel):
    frage_ist_in_domänenwissen: bool = Field(..., description="") # TODO
    relevante_domänen_indexe: List[int] = Field(..., description="") # TODO - restrict to max number of references and restrict to length of domain knowledge

class DA_Answer_ResponseFormat(BaseModel):
    rechtfertigung_aus_kontext: str = Field(..., description="") # TODO
    antwort: str = Field(..., description="") # TODO

class DA_State(BaseModel):
    CurrentConversation: List[Union[HumanMessage, AIMessage]]
    Classification: Optional[DA_DomainClassification_ResponseFormat]
    DomainKnowledge: List[str]

    Answer: Optional[DA_Answer_ResponseFormat]

# ? tool use put into the output format for the prompt ? does that work or is it desirable

class DomainAgent(Agent):
    def __init__(self, llm: ChatOpenAI):

        self.llm = llm
        self.domainClassificationPrompt = genai.load_prompt(DA_DomainClassification_ResponseFormat.__name__)
        self.answerDomainKnowledgePrompt = genai.load_prompt(DA_Answer_ResponseFormat.__name__)

        graph = StateGraph(DA_State)
        graph.add_node(self.DomainKnowledgeClassificationNode.__name__, self.DomainKnowledgeClassificationNode)
        graph.add_node(self.AnswerFromDomainKnowledgeNode.__name__, self.AnswerFromDomainKnowledgeNode)
        graph.add_node(self.IsDomainRelevantNode.__name__, self.IsDomainRelevantNode)
        graph.add_edge(START, self.DomainKnowledgeClassificationNode.__name__)
        graph.add_conditional_edges(
            self.DomainKnowledgeClassificationNode.__name__,
            self.IsDomainRelevantNode,
            {
                True: self.AnswerFromDomainKnowledgeNode.__name__,
                False: END
            }
        )
        graph.add_edge(self.AnswerFromDomainKnowledgeNode.__name__, END)
        self.graph = graph.compile()

        self.state = DA_State(
            CurrentConversation=[],
            Classification=None,
            DomainKnowledge=[],
            Answer=None
        )

        super().__init__(
            llm=llm,
            responseFormats=[
                DA_DomainClassification_ResponseFormat,
                DA_Answer_ResponseFormat
            ]
        )

    def __call__(self):
        self.state = DA_State(**self.graph.invoke(self.state))
        return self.state
    
    def IsDomainRelevantNode(self, state: DA_State):
        assert state.Classification, "Classification should never be none at this point!"
        if not state.Classification.frage_ist_in_domänenwissen:
            return False
        if len(state.Classification.relevante_domänen_indexe) <= 0:
            return False
        return False
    
    def DomainKnowledgeClassificationNode(self, state: DA_State):
        resp = self.llm.with_structured_output(DA_DomainClassification_ResponseFormat).invoke([
            format_system_prompt(
                self.domainClassificationPrompt,
                domainKnowledge=state.DomainKnowledge
            ),
            *DA_State.CurrentConversation
        ])
        resp = DA_DomainClassification_ResponseFormat(**resp)
        return {
            "Classification": resp
        }
        
    def AnswerFromDomainKnowledgeNode(self, state: DA_State):
        assert state.Classification, "Classification should never be none at this point!"
        assert state.Classification.relevante_domänen_indexe, "Viable domain indecies where never provided by the model, this should be caught earlier"

        res = self.llm.with_structured_output(DA_Answer_ResponseFormat).invoke([
            format_system_prompt(
                self.answerDomainKnowledgePrompt, 
                relevantDomainKnowledge=[state.DomainKnowledge[i] for i in state.Classification.relevante_domänen_indexe]
            ),
            *state.CurrentConversation
        ])
        state.Answer = DA_Answer_ResponseFormat(**res)

        return state


    # def ConversationNode(self, state: DA_State):
    #     # get conversation with conversation agent
    #     # get last query
    #     # do a short analysis on what to do better
    #     pass
