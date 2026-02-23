from pydantic import BaseModel, Field
from langgraph.


from ..abstract import Agent


# class RA_OF_Thinking(BaseModel):
#     gedanken: List[str] = Field(..., description="Die Gedanken mit welchen du eine logische rückführung formulierst, informationen über den sachverhalt kompilierst und dir eine Übersicht über die datenlage verschaffst.")

# class RA_OF_Answer(BaseModel):
#     antwort: str = Field(..., description="Die Antwort auf eine informations-anfrage in welcher du dich auf den gegebenen Sachverhalt beziehst.")


# class GedankengangBewertung(TypedDict):
#     bezug_auf_quellen: Literal["6", "5", "4", "3", "2", "1"] # hat der agent sich auf quellen bezogen ?
#     bezug_auf_sachverhalt: Literal["6", "5", "4", "3", "2", "1"] # bezieght sich der agent auf den sachverhalt der frage ?
#     gedankengang_effizienz: Literal["6", "5", "4", "3", "2", "1"] # haellt sich der agent kurz und zum punkt ?
#     korrektur: str


# class WissensAgentState(TypedDict):
#     konversation: List[Union[HumanMessage, AIMessage]]
#     klassifikation: KonversationKlassifikation | None
    
#     llm_calls: int
#     dokument_elemente_in_kontext: List[DocumentChunk] | None

#     # bewertung
#     gedankengang: Gedankengang | None
#     beispiele: List[Gedankengang]

class ReasoningOutput(BaseModel):
    keypoints: str = Field(..., description="")
    gedankengang: str = Field(..., description="")
    antwort: str = Field(..., description="")

class RA_State(BaseModel):
    Answer: ReasoningOutput


class ReasoningAgent(Agent):
    def __init__(self, llm: ChatOpenAI)

    def gedanken_node(self, state: WissensAgentState):

        assert state["klassifikation"], "klassifikation muss stadtgefunden haben zu diesem zeitpunkt"

        bewertung_llm = self.llm.with_structured_output(GedankengangBewertung)

        user_frage = state['klassifikation']["aktuelle_frage"]

        gedanken = self.llm.invoke(
            get_prompt("THINKING_PROMPT").format(
                user_frage = user_frage
            )
        )
        
        antwort = self.llm.invoke(
            get_prompt("ANSWER_PROMPT").format(
                user_frage = user_frage,
                gedanken = gedanken
            )
        )

        string_formatierte_beispiele = '\n'.join([
            f"""
                'frage': {beispiel['frage']}
                'gedanken': {beispiel['gedanken']}
                'bezug_auf_quellen': {beispiel['bewertung']['bezug_auf_quellen']}
                'bezug_auf_sachverhalt': {beispiel['bewertung']['bezug_auf_sachverhalt']}
                'gedankengang_effizienz': {beispiel['bewertung']['gedankengang_effizienz']}
            """
            for beispiel in state['beispiele']
        ])

        bewertung = bewertung_llm.invoke()

        print(f"[DEBUG] - bewertung: {bewertung}")

        return {
            "konversation": [*state["konversation"], antwort],
            "llm_calls": state["llm_calls"] + 1,
            "gedankengang": {
                "frage": AIMessage(user_frage),
                "gedanken": gedanken,
                "antwort": antwort,
                "bewertung": bewertung
            }
        }
