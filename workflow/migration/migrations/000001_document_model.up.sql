-- HUBS

CREATE TABLE IF NOT EXISTS Hub_Document (
    UUID UUID PRIMARY KEY NOT NULL,
    LDTS timestamptz NOT NULL,
    UDTS timestamptz NOT NULL
);

CREATE TABLE IF NOT EXISTS Hub_Document_Page (
    UUID UUID PRIMARY KEY NOT NULL,
    LDTS timestamptz NOT NULL,
    UDTS timestamptz NOT NULL,
    DocumentPageIndex int2 NOT NULL
);

CREATE TABLE IF NOT EXISTS Hub_Text_Chunk (
	UUID UUID PRIMARY KEY NOT NULL,
	LDTS timestamptz NOT NULL,
    UDTS timestamptz NOT NULL,
	Content text NOT NULL
);

CREATE TABLE IF NOT EXISTS Hub_Vectorization (
    UUID UUID PRIMARY KEY NOT NULL,
    LDTS timestamptz NOT NULL,
    UDTS timestamptz NOT NULL
);

-- SATELITES

CREATE TABLE IF NOT EXISTS Sat_Document_Metadata (
    DocumentUUID UUID PRIMARY KEY NOT NULL,
    LDTS timestamptz NOT NULL,
    UDTS timestamptz NOT NULL,

    DocumentHash varchar(128) NOT NULL,
    Name varchar(250) NOT NULL
);

-- LINKS

CREATE TABLE IF NOT EXISTS Lnk_Document_Document_Page (
    DocumentUUID UUID NOT NULL,
    DocumentPageUUID UUID NOT NULL,
    LDTS timestamptz NOT NULL,

    PRIMARY KEY (DocumentUUID, DocumentPageUUID)
);

CREATE TABLE IF NOT EXISTS Lnk_Document_Text_Chunk (
    DocumentUUID UUID NOT NULL,
    TextChunkUUID UUID NOT NULL,
    LDTS timestamptz NOT NULL,

    PRIMARY KEY (DocumentUUID, TextChunkUUID)
);

CREATE TABLE IF NOT EXISTS Lnk_Text_Chunk_Vectorization (
    TextChunkUUID UUID NOT NULL,
    VectorizationUUID UUID NOT NULL,
    LDTS timestamptz NOT NULL,

    PRIMARY KEY (TextChunkUUID, VectorizationUUID)
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
