import os 
from langchain_community.retrievers import AzureAISearchRetriever
from typing import List
from langchain.tools import tool

from datastructure import DocumentChunk


# TODO - generalize the tool calls to using tool calling strategy so that models that dont have native structured output can also use tools
# class ProductReview(BaseModel):
#     """Analysis of a product review."""
#     rating: int | None = Field(description="The rating of the product", ge=1, le=5)
#     sentiment: Literal["positive", "negative"] = Field(description="The sentiment of the review")
#     key_points: list[str] = Field(description="The key points of the review. Lowercase, 1-3 words each.")

# agent = create_agent(
#     model="gpt-5",
#     tools=tools,
#     response_format=ToolStrategy(ProductReview)
# )

@tool(name_or_callable="dokument_suche_werkzeug", description="Ein Werkzeug für die dokument-element-suche durch das mit hilfe von `suchanfrage`, `top_k` Dokument")
def dokument_suche_werkzeug(suchanfrage: str, top_k: int) -> List[DocumentChunk]:

    top_k = max(0, min(top_k, 10))

    azure_dokumentensuche = AzureAISearchRetriever(
        top_k = top_k,
        api_key=os.environ["AZURE_AI_SEARCH_API_KEY"],
        service_name=os.environ["AZURE_AI_SEARCH_SERVICE_NAME"],
        index_name=os.environ["AZURE_AI_SEARCH_INDEX_NAME"],
        content_key="chunk"
    )

    retrieved = azure_dokumentensuche.invoke(suchanfrage)

    retrieved = [r.model_dump() for r in retrieved]

    for r in retrieved:
        assert "page_content" in r, "'page_content' ist nicht der kontent key des vektorsuchen-outputs, bitte den verwendeten key der Vektorsuche fuer text kontent angeben"
        assert "title" in r["metadata"], "'title' ist nicht der quellen key des vektorsuchen-outputs, bitte den verwendeten key der Vektorsuche fuer text quelle angeben"

    return [DocumentChunk(text = r["page_content"], quelle = r["metadata"]["title"]) for r in retrieved]
