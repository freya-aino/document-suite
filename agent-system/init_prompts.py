import mlflow
import dotenv
import os
from mlflow import genai
from typing import List, Tuple

from src.datastructure import *


# FRAGETOOL = Ein Tool mit welchem du mittels der parameter 'search_query' und 'top_k' genau 'k' (anzahl) TEXTSEQUENZ aus der WISSENSDATENBANK, anhand der frage in 'search_query', extrahieren kannst.",
# USERFRAGE = Eine Frage des USER;
# USERFRAGE_ELEMENT = Ein Element der User Frage welches für dich Relevant ist und Vorverarbeitet wurden, sie werden gekenzeichnet und sind die Wichtigsten Informationen für deine AUFGABE;
# WISSENSDATENBANK = Eine Datenbank welche informationen über alle notwendigen Themen enthällt mit welcher du deine AUFGABE bewältigst;
# DOKUMENT = Ein Dokument das in der WISSENSDATENBANK Enthalten ist, dessen Informationen als 100% Wahr und Genau behandelt werden;
# TEXTSEQUENZ = Ein Text welcher direkt aus einem DOKUMENT stammt, diese Informationen sind mit höchster präferenz für richtigkeit und genaugkeit zu werten;
# QUELLE = Eine Referenz designiert mit Wissenschaftlicher notation ([1], [2], etc.) sowie eine Zuordnung der Referenz zu einer TEXTSEQUENZ und DOKUMENT;
# TEAM = Wichtige Mitarbeitende welche einzelne speziealisierte Aufgaben erledigen auf wessen informationen du entweder zurückgreifen kannst oder welche du kontolieren musst um Effizient deine spezielle AUFGABE zu erfüllen;

# GLOBALE_SYSTEMPROMPTS = [
#     "Wörter die nur mit großbustaben geschrieben werden beschreiben immer die selbe entität (z.b. eine Datenstruktur, ein bestimmter Text)",
#     "Du erhälst ein Wörterbuch mit einer liste an Bezeichnungen die Global, in jeder situation gültig sind.",
#     "Du bist Teil eines Teams welches Dokumente verarbeitet",
#     "Eure TEAM AUFGABE ist es zusammen eine akurate und präzise Auskunft zu Fragen zu formulieren die in den zu verwaltenen Dokumenten nach Antworten zu suchen.",
#     "Jeder von euch in TEAM erhält eine eigene AUFGABE",
#     "Die persönliche AUFGABE hat primäre präferenz.",
#     "Die TEAM AUFGABE hat sekundäre preferenz.",
# ]

# WISSENSAGENT_REASONING_PROMPTS = [
#     "AUFGABE: Du erhälst USERFRAGE_ELEMENTE und benutze FRAGETOOL um beliebig viele (TEXTSEQUENZ, QUELLE) Paare zu Erhalten welche relevant for USERFRAGE_ELEMENTE sind.",
#     "Verwende auschließlich Informationen vom FRAGETOOL",
#     "Beantworte die USERFRAGE_ELEMENTE nicht direkt sondern schreibe deinen Gedankengang in welchem du eine Logische rückführung ermittelst wie und wo die TEXTSEQUENZ und QUELLE informationen die elemente der USERFRAGE_ELEMENTE beantworten.",
#     "Schreibe diesen gedankengang in 3-4 Paragraphen mit dem titel 'Gedanken'",
#     "Verwende QUELLE für jeden Bezug auf TEXTSEQUENZ.",
# ]

# WISSENSAGENT_ANTWORT_PROMPTS = [
#     "AUFGABE: Du erhälst einen Gedankengang, gekenzeichnet mit 'Gedanken' und eine liste an quellen gekenzeichnet mit 'Quellen' der auf der basis von TEXTSEQUENZ produziert wurde, und beantworte die USERFRAGE_ELEMENTE.",
#     "Beziehe dich in deiner Antwort ausschließlich auf die Logik die in den Gedanken erläutert wurden, sowie die informationen aus der WISSENSDATENBANK und TEXTSEQUENZ",    "Formuliere deine Antwort in 1-2 Paragraphen",
#     "Beziehe dich auf QUELLE für jedes TEXTSEQUENZ die erwähnt wurde.",
# ]

# WOERTERBUCH_FORMATIERT = [f"{WOERTERBUCH_NOTATION[0]}{p}{WOERTERBUCH_NOTATION[1]}" for p in WOERTERBUCH]
# GLOBALE_SYSTEMPROMPTS_FORMATIERT = [f"{GLOBALE_SYSTEMPROMPTS_NOTATION[0]}{p}{GLOBALE_SYSTEMPROMPTS_NOTATION[1]}" for p in GLOBALE_SYSTEMPROMPTS]
# WISSENSAGENT_REASONING_PROMPTS_FORMATIERT = [f"{AGENT_PROMPT_NOTATION[0]}{p}{AGENT_PROMPT_NOTATION[1]}" for p in WISSENSAGENT_REASONING_PROMPTS]
# WISSENSAGENT_ANTWORT_PROMPTS_FORMATIERT = [f"{AGENT_PROMPT_NOTATION[0]}{p}{AGENT_PROMPT_NOTATION[1]}" for p in WISSENSAGENT_ANTWORT_PROMPTS]



# USER_FRAGE_ELEMENTE_NOTATION = "<user_frage_element>", "</user_frage_element>"
# WOERTERBUCH_NOTATION = "<wörterbuch>", "</wörterbuch>"
# GLOBALE_SYSTEMPROMPTS_NOTATION = "<system>", "</system>"
# AGENT_PROMPT_NOTATION = "<agent>", "</agent>"
# DOKUMENT_RAG_TOOL_NAME = "retreive_document_info"



# ANSWER_QUESTION = """
# - Eine User Frage
# - Ein Gedankengang in welchem die rueckfuehrung der Logik die die Frage beantworten soll beschrieben ist.
# - eine liste an Dokumentausschnitten und quellen.

# USER_FRAGE: {{ user_frage }}
# DOKUEMNTEAUSSCHNITTE und QUELLEN: {state['dokument_elemente_in_kontext']}
# GEDANKENGANG: {{ gedanken }} 

# Du beantwortest die gegebene USER_FRAGE anhand der DOKUMENTAUSSCHNITTE, deren QUELLEN und den GEDANKENGANG.
# Antworte kurz, in einem Paragraphen.
# Verwende in deiner Antwort die quellenangaben aus DOKUMENTENAUSSCHNITTE und QUELLEN die du verwendest mit der notation [QUELLE].
# """

# CONVERSATION_PROMPT = """
# Du fuehrst eine Konversation mti einem User.
# Das Ziehl der KOnversation ist es eine frage zu beantworten.
# Der User sollte eine Frage stellen, bitte ihn/sie freundlicherweise darum.
# """

# DOCUMENT_SEARCH_PROMPT = """
# Du kriegst eine User Frage sowie informationionen ueber diese Frage.

# USER_FRAGE: {{ user_frage }}
# Information ueber USER_FRAGE: {{ user_frage_infos }}
                                            
# Du hast ein Werkzeug 'dokument_suche_werkzeug'.
# Mittels dieses Werkzeuges formuliere eine 'suchanfrage' um 'top_k' Dokumentauszuege zu finden die fuer die beantwortung der USER_FRAGE relevant sind.
# Die 'suchanfrage' sollte die USER_FRAGE beschreiben, die Informationen mit beinhalten, und fuer eine Vektor suche ausgelegt sein.
# Die 'suchanfrage' wird via Vektorsuche an ein LLM gegeben um relevante Dokumentauszuege zu finen, beachte die formatierung die solche LLM-encoder erwarten.
# Diese Dokumentauszuege sind relevant zur beantwortung der USER_FRAGE.
# Benutze hierbei 'top_k' um bei praeziesen Fragen weniger Dokumentauszuege zu kriegen, und bei offeneren Fragen mehr Dokumentauszuege zu kriegen.
# 'top_k' ist auf einer skala von 1 bis 10, wo 1 = ein Dokumentauszug ist und 10 = viele Dokumentenauszuege.
# """

# THINKING_PROMPT = """
# Du kriegst:
# - Eine user Frage
# - Informationen ueber diese Frage
# - eine liste an dokumentausschnitte und deren quellen
# - fals vorhanden eine Korrektur ueber deine vorherigen versuche.

# USER_FRAGE: {{ user_frage }}
# Information ueber USER_FRAGE: {state['klassifikation']}
# DOKUEMNTEAUSSCHNITTE und QUELLEN: {state['dokument_elemente_in_kontext']}
# KORREKTUR: {state['gedankengang']['bewertung']['korrektur'] if state['gedankengang'] else "KEINE"}.

# Du denks ueber die Frage des Users nach.
# Du versuchst durch die Korrektur vorherige fehler zu vermeiden.
# Du beziehst dich zu jedem moeglichen zeitpunkt auf die quellen und dokumentausscnitte da sie die besten Informationen enthalten.

# Liste dein Gedankengang auf in welchem du alle relevanten informationen durchlaeufst.
# Beantworte die Frage NICHT sondern beschreibe nur den Gedankengang.
# """

# ANSWER_PROMPT = """
# Du kriegst:
# - Eine User Frage
# - Ein Gedankengang in welchem die rueckfuehrung der Logik die die Frage beantworten soll beschrieben ist.
# - eine liste an Dokumentausschnitten und quellen.

# USER_FRAGE: {{ user_frage }}
# DOKUEMNTEAUSSCHNITTE und QUELLEN: {state['dokument_elemente_in_kontext']}
# GEDANKENGANG: {{ gedanken }} 

# Du beantwortest die gegebene USER_FRAGE anhand der DOKUMENTAUSSCHNITTE, deren QUELLEN und den GEDANKENGANG.
# Antworte kurz, in einem Paragraphen.
# Verwende in deiner Antwort die quellenangaben aus DOKUMENTENAUSSCHNITTE und QUELLEN die du verwendest mit der notation [QUELLE].
# """


# REASONING_EVALUATION = """
# Du kriegst:
# - Eine Liste an Beispielen welche du als Wichtigste referenz siehst.
# - Eine User Frage
# - Ein Gedankengang in welchem die rueckfuehrung der Logik die die Frage beantworten soll beschrieben ist.
# - eine liste an Dokumentausschnitten und quellen.
# - Die Antwort auf die User Frage

# ---
# BEISPIElE:
                                
# {string_formatierte_beispiele}

# ---

# USER_FRAGE: {user_frage}
# DOKUEMNTEAUSSCHNITTE und QUELLEN: {state['dokument_elemente_in_kontext']}
# GEDANKENGANG: {gedanken.content} 
# ANTWORT: {antwort.content}

# ---

# Bewerte den Gedankengang anhand des Deutschen Schulnoten Systems fuer die Faktoren.
# "1" bedeutet sehr gut und "6" bedeutet sehr schlecht, alles dazwischen ist eine jehweilige Einstufung der Bewertung.
# Bewerte den GEDANKENGANG nach diesem System auf den die Folgenden Faktoren:
# - bezug_auf_quellen: wie gut bezieht sich der gedankengang auf die vorhandenen quellen, und werden diese sinnvoll genutzt um eine logik zu erklaeren.
# - bezug_auf_sachverhalt: wie gut wird sich auf den sachverhalt der frage bezogen waehrend des gedankengangs.
# - gedankengang_effizienz - wie effizient die gedanken verwendet werden, ob sachen ausgefuehrt werden die irrelevant sind und wie schnell sich auf eine konkrete logik bezogen wird.
# Fuege eine korrektur hinzu in welcher du darauf hinweist wieso du gute oder schlechte noten verteilt hast, diese sollen so formuliert werden das der GEDANKENGANG genau weis was die fehler / probleme waren.
# Bewerte den GEDANKENGANG anhand der BEISPIELE, diese Beispiele haben eine Perfekte Notenvergabe welche du emulieren solltest. 
# """


CA_P_Conversation = """
Du führst eine Konversation mit einem User.
Das Ziel der Konversation ist es so viele Informationenbits über die Frage / Fragen oder Umstände des Users zu erfragen.
Wiederhohle nie Informationen die bereits in deinen Informationenbits existieren .

Führe die Konversation so das du alle folgende Konversationspunkte erfragt hast und einer zureichende Antwort erhalten hast:
{% for qg in questionairGoals %}
- {{ qg }}
{% endfor %}

Folgende Informationenbits hast du bereits sammeln können:
{% for i in informationBits %}
- {{ i }}
{% endfor %}
"""

CA_P_Classification = """
Klassifiziere den letzten User Text einer Konversation.
Letzter User Text:
- {{ lastUserMessage }}
"""

def saveAllPromptsToMLFlow(prompts: List[Tuple], lang: str = "de"):
    for (promptName, promptTemplate, responseFormat) in prompts:
        genai.register_prompt(
            name=promptName,
            template=promptTemplate,
            response_format=responseFormat,
            commit_message="Initial commit",
            tags={
                "language": lang,
            },
        )

if __name__ == "__main__":

    # load environment variables
    dotenv.load_dotenv()

    # initialize mlflow
    mlflow.set_tracking_uri(f"http://{os.environ['MLFLOW_HOST']}:{os.environ['MLFLOW_PORT']}") 
    mlflow.set_experiment("test experiment") # TODO - dont hardcode experiment
    print("mlflow - initialized")

    saveAllPromptsToMLFlow([
        ("CA_CONVERSATION", CA_P_Conversation, CA_OF_Conversation),
        ("CA_CLASSIFICATION", CA_P_Classification, CA_OF_Classification),
    ])