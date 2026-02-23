import mlflow

from pydantic import BaseModel, Field
from langgraph.graph import StateGraph

from ..abstract import Agent



class SearchQueryRefinement_OutputFormat(BaseModel):
    keyword_suche: str = Field(..., description="Die such Query des users wird umformuliert so das sie auschließlich Informationen zu den verwendeten keywords in der Wissensdatenbank sucht.")
    kontext_suche: str = Field(..., description="Die such Query des users wird umformuliert so das sie im kontext der Frage relevante Informationen aus der Wissensdatenbank sucht.")
    auser_kontext_suche: str = Field(..., description="Die such Query des users wird umformuliert so das sie auserhalb des kontexts der Frage, mit sekundärem bezug auf den Kontext, Informationen aus der Wissensdatenbank sucht. ")
    anzahl_dokument_elemente: int = Field(..., description="Die Anzahl (0-3) an Dokument elementen welche aus der Wissensdatenbank gehohlt werden.", ge=0, le=3)

class DocumentSearch(BaseModel):
    DocumentChunk: str
    Source: str

class KA_State(BaseModel):
    OriginalQuery: str
    RefinedQuery: SearchQueryRefinement_OutputFormat
    DocumentChukContext: list[DocumentSearch]
    

class KnowledgeAgent(Agent):

    def __init__(self):
        # max_llm_calls: int = 10
        # erwuenschte_note: float = 3.0
        self.QueryRefinementPrompt = genai.load_prompt(KA_DomainClassification_ResponseFormat.__name__)


        graph = StateGraph(KA_State)
        # graph.add_node(self.dokument_suche_werkzeug_node.__name__, self.dokument_suche_werkzeug_node)
        # graph.add_node(self.gedanken_node.__name__,                self.gedanken_node)
    
        # graph.add_edge(START, self.user_input_node.__name__)
        # graph.add_conditional_edges(
        #     self.user_input_node.__name__, 
        #     self.konversation_hat_frage,
        #     {
        #         "ja": self.dokument_suche_werkzeug_node.__name__, 
        #         "nein": self.konversation_node.__name__
        #     }
        # )
        # graph.add_edge(self.konversation_node.__name__, self.user_input_node.__name__)
        
        # graph.add_edge(self.dokument_suche_werkzeug_node.__name__, self.gedanken_node.__name__)
        # graph.add_edge(self.gedanken_node.__name__, END)
        # graph.add_conditional_edges(
        #     self.gedanken_node.__name__,
        #     self.ist_gedankengang_zu_ende,
        #     {
        #         True: END, 
        #         False: self.dokument_suche_werkzeug_node.__name__, 
        #     }
        # )

        # workflow.add_edge(gedanken_node.__name__, user_input_node.__name__)
        # TODO : vieleicht fuegen wir den bewertungs node und den feedback node nach dem gedanke node, vor den antwort node ?

        self.graph = graph.compile()

    def dokument_suche_werkzeug_node(self, state: WissensAgentState):
        
        assert state['klassifikation'], "klassifikation muss geschehen sein bevor das hier gecalled wird"

        user_frage = state['klassifikation']["aktuelle_frage"]
        user_frage_infos = {
            "fachbereich": state['klassifikation']['fachbereich'],
            "frage_praezision": state['klassifikation']['frage_praezision'],
            "frage_typ": state['klassifikation']["frage_typ"]
        }

        dokument_suche_llm = self.llm.bind_tools([dokument_suche_werkzeug])

        prompt = get_prompt("DOCUMENT_SEARCH_PROMPT")
        werkzeug_ausgabe = dokument_suche_llm.invoke(prompt)

        has_valid_tool_call = len(werkzeug_ausgabe.tool_calls) > 0 and werkzeug_ausgabe.tool_calls[-1]["name"] == 'dokument_suche_werkzeug'
        if has_valid_tool_call:
            print(f"[DEBUG] - werkzeugaufruf mit {werkzeug_ausgabe.tool_calls[-1]['args']}")
            ergebnisse = dokument_suche_werkzeug.invoke(werkzeug_ausgabe.tool_calls[-1]["args"])

            # TODO format ergebnise

            return {
                "dokument_elemente_in_kontext": ergebnisse
            }
        return {}


    @mlflow.trace(name="KA_QueryRefinementNode", span_type="func")
    def QueryRefinementNode(self, state: KA_State):
        
        systemQueries = self.llm.with_structured_output(self.QueryRefinementPrompt.response_format).invoke([
            {
                "role": "system",
                "content": self.QueryRefinementPrompt.format(
                    allow_partial=False,
                    documentSystemKnowledge=[], # TODO
                )
            }
        ])
        systemQueries = SearchQueryRefinement_OutputFormat(**systemQueries)

        return {
            "SystemSearchQueries": systemQueries,
        }

    # def ReasoningNode(self, state: KA_State):
    #     # evalueate if thinking is nessesary
    #     pass
    