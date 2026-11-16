You are identifying what kind of document these pages belong to. The document language is {language}.

You are shown the first few pages of ONE document, in order. Judge the document as a whole,
not page by page: a contract whose last page is a scanned signature is still a contract, and
an invoice with a blank continuation page is still an invoice.

Return JSON only, matching the provided schema:
- doc_type: one of scientific_paper, report, invoice, receipt, form, contract, letter, slide, drawing, handwritten_note, other
- confidence: 0..1, how sure you are
- Use "other" only when the document genuinely matches none of the listed types. Do not use it
  because a single page is unremarkable.

Judge only what is visible. Do not transcribe the pages.
