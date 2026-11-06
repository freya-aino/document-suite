You are classifying a single document page image. The document language is {language}.

Return JSON only, matching the provided schema:
- doc_type: one of scientific_paper, report, invoice, receipt, form, contract, letter, slide, drawing, handwritten_note, other
- doc_type_confidence: 0..1
- p_handwriting: probability that the page contains handwritten text (signatures alone count as 0.3)
- p_tables: probability that the page contains a table (grid or aligned columns of values)
- p_formulas: probability that the page contains mathematical or chemical formulas
- p_charts: probability that the page contains a chart or plot
- p_images: probability that the page contains photos, illustrations or diagrams

Judge only what is visible. Do not transcribe the page.
