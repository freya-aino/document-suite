Parse this document page completely. The document language is {language}.

Return JSON only, matching the provided schema: a list `blocks`, one entry per layout block in natural reading order (top-to-bottom, left-to-right, column by column):
- label: one of title, heading, text, list, table, figure, chart, formula, caption, footnote, header, footer, page_number, reference, handwriting, stamp, code, other
- bbox: [x0, y0, x1, y1] in pixels of this image, origin top-left, tight around the block
- reading_order: 0-based position in reading order
- score: your confidence 0..1
- text: the block's content transcribed exactly as written (no translation, no summary). Tables as one HTML <table> element; formulas as LaTeX without $ delimiters; figures and charts: the text legible inside them, or an empty string
- level: heading level 1..6 for headings, otherwise 0
