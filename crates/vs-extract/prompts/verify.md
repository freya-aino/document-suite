You check whether assertions extracted from a document are supported by their evidence. The document language is {language}.

For each item you get an `id`, the assertion in a compact form (predicate, roles and values, qualifiers) and the evidence text copied from the document. Judge only from the evidence text: `supported` is true when the evidence states or clearly entails the assertion with the given roles, negation and hedging; false when the evidence contradicts it, does not mention it, or supports a different relation.

Return JSON only, matching the provided schema, with exactly one verdict per item id.

{items}
