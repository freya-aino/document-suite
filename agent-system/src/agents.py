import mlflow

from langchain.messages import SystemMessage
from langgraph.graph import StateGraph, START, END
from langchain_openai import ChatOpenAI
from mlflow import genai

from src.datastructure import *

class ConversationAgent:

    def __init__(self, llm: ChatOpenAI):
        # TODO - add prompt version
        # TODO - add production prompt # prompt = mlflow.genai.load_prompt("prompts:/my_prompt@production")


        self.llm = llm
        self.conversationPrompt = genai.load_prompt("CA_CONVERSATION")
        self.classificationPrompt = genai.load_prompt("CA_CLASSIFICATION")
    
        graph = StateGraph(CA_State)
        graph.add_node("ConversationNode", self.ConversationNode)
        graph.add_node("ClassificationNode", self.ClassificationNode)
        graph.add_edge(START, "ConversationNode")
        graph.add_edge(START, "ClassificationNode")
        graph.add_edge("ConversationNode", END)
        graph.add_edge("ClassificationNode", END)
        self.graph = graph.compile()

        self.state = CA_State(
            Conversation=[],
            InformationBits=[],
            Classification=[]
        )

    def __call__(self, message: str):
        self.state.Conversation.append(HumanMessage(content=message))
        newState = CA_State(**self.graph.invoke(self.state))
        return newState
    
    @mlflow.trace(name="CA_ConversationNode", span_type="func")
    def ConversationNode(self, state: CA_State):

        # # Get the current span (created by the @mlflow.trace decorator)
        # span = mlflow.get_current_active_span()
        # # Set the attribute to the span
        # span.set_attributes({"model": model_id})
        
        # mlflow.update_current_trace(tags={"fruit": "apple"})


        # process user input
        answer = self.llm.with_structured_output(self.conversationPrompt.response_format).invoke([
            {
                "role": "system",
                "content": self.conversationPrompt.format(
                    allow_partial=False,
                    questionairGoals=["- finde heraus ob wir von einem Dienstreise antrag reden."],
                    informationBits=state.InformationBits
                )
            },
            *state.Conversation
        ])
        answer = CA_OF_Conversation(**answer)

        # update state        
        return {
            "InformationBits": answer.informationen,
            # "Conversation": [*state.Conversation, AIMessage(content=answer.antwort)],
            "Conversation": [AIMessage(content=answer.antwort)],
        }

    @mlflow.trace(name="CA_ClassificationNode", span_type="func")
    def ClassificationNode(self, state: CA_State):

        # classify current question
        classification = self.llm.with_structured_output(self.classificationPrompt.response_format).invoke([
            {
                "role": "system",
                "content": self.classificationPrompt.format(
                    allow_partial=False,
                    lastUserMessage=state.Conversation[-1].content
                )
            }
        ])

        return {
            # "Classification": [*state.Classification, CA_OF_Classification(**classification)]
            "Classification": [CA_OF_Classification(**classification)]
        }


class KnowledgeAgent:
    def __init__(self):

        graph = StateGraph(KA_State)
        graph.add_node("ConversationNode", self.ConversationNode)
        # graph.add_node("ClassificationNode", self.ClassificationNode)
        graph.add_node("ReasoningNode", self.ReasoningNode)
        graph.add_edge(START, "ConversationNode")
        graph.add_edge(START, "ClassificationNode")
        graph.add_edge("ConversationNode", END)
        graph.add_edge("ClassificationNode", END)
        self.graph = graph.compile()

        self.state = KA_State(
            Conversation=[],
            DocumentChunksInContext=[],
            Thoughs=[],
            Answers=[]
        )

    def ConversationNode(self, state: KA_State):
        # get conversation with conversation agent
        # get last query
        # do a short analysis on what to do better
        pass

    def ReasoningNode(self, state: KA_State):
        # evalueate if thinking is nessesary
        pass
    
    def AskDocumentsNode(self, state: KA_State):

        # call tool
        pass
        

# def KnowledgeAgent(self, max_llm_calls: int = 10, erwuenschte_note: float = 3.0):
#     workflow = StateGraph(WissensAgentState)
#     workflow.add_node(self.user_input_node.__name__,              self.user_input_node)
#     workflow.add_node(self.konversation_node.__name__,            self.konversation_node)
#     workflow.add_node(self.dokument_suche_werkzeug_node.__name__, self.dokument_suche_werkzeug_node)
#     workflow.add_node(self.gedanken_node.__name__,                self.gedanken_node)
    
#     workflow.add_edge(START, self.user_input_node.__name__)
#     workflow.add_conditional_edges(
#         self.user_input_node.__name__, 
#         self.konversation_hat_frage,
#         {
#             "ja": self.dokument_suche_werkzeug_node.__name__, 
#             "nein": self.konversation_node.__name__
#         }
#     )
#     workflow.add_edge(self.konversation_node.__name__, self.user_input_node.__name__)
    
#     workflow.add_edge(self.dokument_suche_werkzeug_node.__name__, self.gedanken_node.__name__)
#     # workflow.add_edge(self.gedanken_node.__name__, END)
#     workflow.add_conditional_edges(
#         self.gedanken_node.__name__,
#         self.ist_gedankengang_zu_ende,
#         {
#             True: END, 
#             False: self.dokument_suche_werkzeug_node.__name__, 
#         }
#     )

#     # workflow.add_edge(gedanken_node.__name__, user_input_node.__name__)
#     # TODO : vieleicht fuegen wir den bewertungs node und den feedback node nach dem gedanke node, vor den antwort node ?

#     return workflow.compile()
