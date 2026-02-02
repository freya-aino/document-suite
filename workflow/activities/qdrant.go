package activities

import (
	"context"
	src "workflow/src"

	"github.com/qdrant/go-client/qdrant"
)

func QdrantCreateCollection(ctx context.Context, collectionName string, vectorSize uint64, distance qdrant.Distance) error {

	client, err := src.QdrantClient()
	if err != nil {
		return err
	}
	defer client.Close()

	client.CreateCollection(context.Background(), &qdrant.CreateCollection{
		CollectionName: collectionName,
		VectorsConfig: qdrant.NewVectorsConfig(&qdrant.VectorParams{
			Size:     vectorSize,
			Distance: distance,
		}),
	})

	return nil
}

func QdrantPutIntoCollection(ctx context.Context, collectionName string, documentChunk src.DocumentChunk, vector []float32) error {

	client, err := src.QdrantClient()
	if err != nil {
		return err
	}
	defer client.Close()

	point := qdrant.UpsertPoints{
		CollectionName: collectionName,
		Points: []*qdrant.PointStruct{
			{
				Id:      qdrant.NewIDUUID(documentChunk.UUID.String()),
				Vectors: qdrant.NewVectors(vector...),
				Payload: qdrant.NewValueMap(map[string]any{
					"value":              documentChunk.Content,
					"UUID":               documentChunk.UUID.String(),
					"ParentDocumentUUID": documentChunk.ParentDocumentUUID.String(),
				}), // TODO - add functionality for encoding "chunk type" and other atributes
			},
		},
	}

	_, err = client.Upsert(ctx, &point)
	if err != nil {
		return err
	}

	// TODO - check opInfo for correct operation procedure

	return nil
}

// TODO - mainly for collection exists or similar parameters
// func GetCollectionInfo(ctx context.Context, collectionName string) {
// 	client, err := src.QdrantClient()
// 	if err != nil {
// 		return err
// 	}
// 	defer client.Close()
// 	info, err := client.GetCollectionInfo(context.Background(), collectionName)
// 	// info.Status
// 	// info.IndexedVectorsCount
// 	// info.PointsCount
// 	// info.SegmentsCount
// }

func QdrantQueryCollection(ctx context.Context, collectionName string, queryVector []float32) ([]src.VectorQueryResult, error) {

	client, err := src.QdrantClient()
	if err != nil {
		return nil, err
	}
	defer client.Close()

	searchResult, err := client.Query(context.Background(), &qdrant.QueryPoints{
		CollectionName: collectionName,
		Query:          qdrant.NewQuery(queryVector...),
		// Filter: , // TODO - add a filter option (for example for filtering for document, document type, etc.)
	})

	var results []src.VectorQueryResult
	for _, ell := range searchResult {
		// ell.OrderValue
		// ell.Payload
		// ell.Score
		// ell.Version
		// ell.Id
		// ell.ShardKey
		// ell.Vectors

		uuid, err := src.UUIDFromString(ell.Payload["UUID"].GetStringValue())
		if err != nil {
			return nil, err
		}
		parentUuid, err := src.UUIDFromString(ell.Payload["ParentDocumentUUID"].GetStringValue())
		if err != nil {
			return nil, err
		}
		content := ell.Payload["value"].GetStringValue()

		results = append(results, src.VectorQueryResult{
			Similarity: ell.Score,
			DocumentChunk: src.DocumentChunk{
				UUID:               uuid,
				ParentDocumentUUID: parentUuid,
				Content:            content,
			},
		})
	}

	return results, nil
}

// client.UpdateVectors()
