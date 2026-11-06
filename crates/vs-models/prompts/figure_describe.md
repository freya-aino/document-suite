Describe this figure from a document. The document language is {language}; write the description in that language.

Return JSON only, matching the provided schema:
- description: a factual description of what the figure shows (2-6 sentences). Do not speculate beyond what is visible.
- figure_type: photo, diagram, flowchart, schematic, map, illustration, screenshot, chart, other
- text_in_image: all legible text inside the figure, in reading order
- entities: named things visible or labelled in the figure (components, people, places, products, variables), each with a short label and type
- relations: explicit relations shown by the figure (arrows, connections, containment), as subject / predicate / object using the entity labels
