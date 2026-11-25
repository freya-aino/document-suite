Convert this chart from a document into data. The document language is {language}.

Return JSON only, matching the provided schema:
- chart_type: bar, line, scatter, pie, area, box, heatmap, other
- title: chart title if visible, else empty
- x_axis / y_axis: label and unit if visible
- table_html: an HTML <table> with one header row containing the series values read from the chart. Use the axis labels and legend names exactly as printed. Mark estimated values read from bar heights or line positions with a trailing "~".
- description: one or two sentences stating the main trend or comparison shown.
