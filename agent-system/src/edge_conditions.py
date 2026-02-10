
def konversation_hat_frage(self, state: WissensAgentState):
    assert state["klassifikation"], "die konversation muss klassifiziert worden sein"
    print(f"[DEBUG] - Konversation hat frage: {state['klassifikation']['hat_frage']}")
    return state["klassifikation"]["hat_frage"]

def ist_gedankengang_zu_ende(self, state: WissensAgentState):
            
    assert state['gedankengang'], "gedankengang sollte gesetzt sein an diesem punkt"

    note = sum([
        int(state['gedankengang']['bewertung']["bezug_auf_quellen"]),
        int(state['gedankengang']['bewertung']["bezug_auf_sachverhalt"]),
        int(state['gedankengang']['bewertung']['gedankengang_effizienz'])
    ]) / 3

    print(f"[DEBUG] - note ereicht: {note}")

    if note <= self.erwuenschte_note:
        return True

    if state['llm_calls'] >= self.max_llm_calls:
        print("[DEBUG] - maximale anzahl an llm calls ereicht")
        return True
    
    return False
    
