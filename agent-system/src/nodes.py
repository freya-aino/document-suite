from langchain.messages import HumanMessage, AIMessage, SystemMessage
from prompts import get_prompt
from langchain_openai import ChatOpenAI

from datastructure import ConversationAgentState



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
