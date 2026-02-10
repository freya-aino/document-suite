from langchain.messages import HumanMessage, AIMessage

from typing import Optional, List, Union, Annotated
import operator
from pydantic import BaseModel, Field



class CA_OF_Classification(BaseModel):
    ist_frage: bool = Field(..., description="Ist eine Frage gestellt worden")
    frage_präzision: int = Field(..., description="Die präzision einer gestellten frage (1-10)", ge=1, le=10)
    frage_fachgebiet: str = Field(..., description="Der Fachgebiet der Frage.")

class CA_OF_Conversation(BaseModel):
    antwort: str = Field(..., description="Deine Antwort die die Konversation weiterführt und mehr informationen sammelt und Unklarheiten aufkößt.")
    informationen: Annotated[List[str], operator.add] = Field(..., description="Die Informationen oder Key-points welche du von der bisherigen Konversation erhalten kontest.")


class CA_State(BaseModel):
    Conversation: Annotated[List[Union[HumanMessage, AIMessage]], operator.add]
    InformationBits: Annotated[List[str], operator.add]
    Classification: Annotated[List[CA_OF_Classification], operator.add]




class KA_OF_Thinking(BaseModel):
    gedanken: List[str] = Field(..., description="Die Gedanken mit welchen du eine logische rückführung formulierst, informationen über den sachverhalt kompilierst und dir eine Übersicht über die datenlage verschaffst.")

class KA_OF_Answer(BaseModel):
    antwort: str = Field(..., description="Die Antwort auf eine informations-anfrage in welcher du dich auf den gegebenen Sachverhalt beziehst.")

class DocumentChunk(BaseModel):
    Text: str
    SourceDocument: str

class KA_State(BaseModel):
    Conversation: List[AIMessage]
    DocumentChunksInContext: List[DocumentChunk]
    Thoughs: List[AIMessage]
    Answers: List[AIMessage]



# class GedankengangBewertung(TypedDict):
#     bezug_auf_quellen: Literal["6", "5", "4", "3", "2", "1"] # hat der agent sich auf quellen bezogen ?
#     bezug_auf_sachverhalt: Literal["6", "5", "4", "3", "2", "1"] # bezieght sich der agent auf den sachverhalt der frage ?
#     gedankengang_effizienz: Literal["6", "5", "4", "3", "2", "1"] # haellt sich der agent kurz und zum punkt ?
#     korrektur: str

# class Gedankengang(TypedDict):
#     frage: AIMessage 
#     gedanken: AIMessage
#     antwort: AIMessage
#     bewertung: GedankengangBewertung

# class WissensAgentState(TypedDict):
#     konversation: List[Union[HumanMessage, AIMessage]]
#     klassifikation: KonversationKlassifikation | None
    
#     llm_calls: int
#     dokument_elemente_in_kontext: List[DocumentChunk] | None

#     # bewertung
#     gedankengang: Gedankengang | None
#     beispiele: List[Gedankengang]

