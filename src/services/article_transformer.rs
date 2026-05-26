//! WPPost -> StrapiArticlePayload conversion, with WP-id -> Strapi-document-id
//! resolution via MappingCache.

use crate::infrastructure::cache::MappingCache;
use crate::models::{
    BlockType, ContentBlock, CoverImagePayload, RegistrationCtaBlock, StrapiArticlePayload, WpPost,
};
use crate::services::excerpt_normalizer::ExcerptNormalizer;

pub struct ArticleTransformer;

impl ArticleTransformer {
    pub fn transform(
        post: &WpPost,
        rebuilt_content: &str,
        blocks: &[ContentBlock],
        cache: &MappingCache,
    ) -> StrapiArticlePayload {
        let cover = CoverImagePayload {
            public: if post.cover_image.id > 0 { Some(post.cover_image.id) } else { None },
            alt_tag: post.cover_image.alt_text.clone(),
        };

        StrapiArticlePayload {
            site: "Daily_Squirt".to_string(),
            title: ExcerptNormalizer::normalize_title(&post.title),
            slug: post.slug.clone(),
            excerpt: ExcerptNormalizer::normalize(&post.excerpt),
            publish_date: post.date.clone(),
            body: rebuilt_content.to_string(),
            ds_performers: Self::extract_performer_ids(&post.tag_ids, cache),
            ds_studios: Self::extract_studio_ids(&post.tag_ids, cache),
            cover_image: cover,
            ds_author: cache.get_author_document_id(post.author_id),
            ds_sub_category: cache.get_category_document_id(post.categories),
            allow_comment: post.comments_enabled,
            registration_cta_block: Self::extract_cta_blocks(blocks),
        }
    }

    fn extract_performer_ids(tag_ids: &[i64], cache: &MappingCache) -> Vec<String> {
        tag_ids.iter().filter_map(|id| cache.get_performer_document_id(*id)).collect()
    }

    fn extract_studio_ids(tag_ids: &[i64], cache: &MappingCache) -> Vec<String> {
        tag_ids.iter().filter_map(|id| cache.get_studio_document_id(*id)).collect()
    }

    fn extract_cta_blocks(blocks: &[ContentBlock]) -> Vec<RegistrationCtaBlock> {
        blocks
            .iter()
            .filter(|b| b.block_type == BlockType::Cta)
            .map(|b| RegistrationCtaBlock {
                enabled: true,
                position_after_paragraph: b.index,
                label: b.text.clone(),
                url: b.href.clone(),
            })
            .collect()
    }
}
