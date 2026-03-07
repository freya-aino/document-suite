-- HUBS

CREATE TABLE IF NOT EXISTS HubDocument (
    UUID UUID,
    LDTS timestamptz,
    UDTS timestamptz
);

CREATE TABLE IF NOT EXISTS HubDocumentPage (
    UUID UUID,
    LDTS timestamptz,
    UDTS timestamptz,
    DocumentPageIndex int2
);

CREATE TABLE IF NOT EXISTS HubTextChunk (
	UUID UUID,
	LDTS timestamptz,
    UDTS timestamptz,
	Content text
);

CREATE TABLE IF NOT EXISTS HubVectorization (
    UUID UUID,
    LDTS timestamptz,
    UDTS timestamptz
);

-- SATELITES

CREATE TABLE IF NOT EXISTS SatDocumentMetadata (
    DocumentUUID UUID,
    LDTS timestamptz,
    UDTS timestamptz,

    DocumentHash varchar(128),
    Name varchar(250)
);


-- LINKS

CREATE TABLE IF NOT EXISTS LnkDocumentDocumentPage (
    DocumentUUID UUID,
    DocumentPageUUID UUID,
    LDTS timestamptz
);

CREATE TABLE IF NOT EXISTS LnkDocumentTextChunk (
    DocumentUUID UUID,
    TextChunkUUID UUID,
    LDTS timestamptz
);

CREATE TABLE IF NOT EXISTS LnkTextChunkVectorization (
    TextChunkUUID UUID,
    VectorizationUUID UUID,
    LDTS timestamptz
);

-- CREATE TABLE IF NOT EXISTS HubVectorQueryResult
-- Similarity    float32       `json:"similarity"`
-- 	// Vector     []float32 `json:"vector"`

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
