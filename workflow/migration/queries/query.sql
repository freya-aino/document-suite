-- name: GetAllDocuments :many
SELECT * FROM Hub_Document;

-- name: GetAllPagesForDocument :many
SELECT Hub_Document_Page.UUID
FROM Hub_Document
JOIN Lnk_Document_Document_Page AS link ON Hub_Document.UUID = Lnk_Document_Document_Page.DocumentUUID
JOIN Hub_Document_Page ON link.DocumentPageUUID = Hub_Document_Page.UUID
WHERE Hub_Document.UUID = $1;
