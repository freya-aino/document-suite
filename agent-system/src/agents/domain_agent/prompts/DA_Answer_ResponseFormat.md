# Aufgabe
Beantworte Fragen des Users anhand des Relevanten Domänen-Wissens.
Beziehe dich in deiner Antwort auf besagtes Relevantes Domänen-Wissen.

# Relevantes Domänen-Wissen:
{% for d in relevantDomainKnowledge %}
- {{ d }}
{% endfor %}