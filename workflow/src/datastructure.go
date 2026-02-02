package src

import (
	"github.com/samborkent/uuidv7"
)

type Document struct {
	UUID uuidv7.UUID `json:"UUID"`
	Name string      `json:"Name"`
}

type DocumentChunk struct {
	UUID               uuidv7.UUID `json:"UUID"`
	ParentDocumentUUID uuidv7.UUID `json:"ParentDocumentUUID"`
	Content            string      `json:"Content"`
}

type VectorQueryResult struct {
	DocumentChunk DocumentChunk `json:"DocumentChunk"`
	Similarity    float32       `json:"similarity"`
	// Vector     []float32 `json:"vector"`
}
