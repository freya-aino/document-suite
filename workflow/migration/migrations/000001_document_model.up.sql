
CREATE TABLE IF NOT EXISTS DocumentHub (
    UUID UUID
);

-- type Document struct {
-- 	UUID uuidv7.UUID `json:"UUID"`
-- 	S3ID string      `json:"S3ID"`
-- 	Name string      `json:"Name"`
-- }

-- type DocumentChunk struct {
-- 	UUID               uuidv7.UUID `json:"UUID"`
-- 	ParentDocumentUUID uuidv7.UUID `json:"ParentDocumentUUID"`
-- 	Content            string      `json:"Content"`
-- }

-- type VectorQueryResult struct {
-- 	DocumentChunk DocumentChunk `json:"DocumentChunk"`
-- 	Similarity    float32       `json:"similarity"`
-- 	// Vector     []float32 `json:"vector"`
-- }
