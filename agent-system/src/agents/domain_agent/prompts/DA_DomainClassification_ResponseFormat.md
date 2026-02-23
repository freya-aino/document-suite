# Aufgabe
Identifiziere in der gegebenen Konversation ob relevante Bezüge auf das dir gegebene Domänen-Wissen existieren.
Stelle sicher das die Konversation Fragen enthällt die sich durch das Gegebene Domänen-Wissen beantworten lassen.
Falls es keine Relevanten Domänen für die Konversation giebt, lege die Anfrage ab.

# Domänen Wissen
{% for d in domainKnowledge %}
- {{ d }}
{% endfor %}
